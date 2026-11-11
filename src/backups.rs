use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use aws_sdk_s3::{
    Client,
    config::{Credentials, Region, timeout::TimeoutConfig},
    primitives::DateTime,
    types::Object,
};
use tokio::{fs, io::AsyncWriteExt, sync::watch};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StorageConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://services.mentormarketplace.ru:9443".into(),
            bucket: "ms-bucket".into(),
            region: "us-east-1".into(),
            access_key: String::new(),
            secret_key: String::new(),
        }
    }
}
impl StorageConfig {
    pub async fn client(&self) -> Result<Client> {
        crate::logging::register_credentials(&self.access_key, &self.secret_key);
        tracing::debug!(target:"app::s3","Preparing S3 connection");
        if self.access_key.is_empty() || self.secret_key.is_empty() {
            bail!("Configure the access key and secret key in Settings → Storage.");
        }
        if !self.endpoint.starts_with("https://") && !self.endpoint.starts_with("http://") {
            bail!("Storage endpoint must start with https:// or http://");
        }
        if self.bucket.trim().is_empty() || self.region.trim().is_empty() {
            bail!("Bucket and region are required");
        }
        Ok(Client::from_conf(
            aws_sdk_s3::config::Builder::new()
                .behavior_version_latest()
                .region(Region::new(self.region.clone()))
                .credentials_provider(Credentials::new(
                    &self.access_key,
                    &self.secret_key,
                    None,
                    None,
                    "application",
                ))
                .endpoint_url(&self.endpoint)
                .force_path_style(true)
                .timeout_config(
                    TimeoutConfig::builder()
                        .connect_timeout(Duration::from_secs(10))
                        .read_timeout(Duration::from_secs(30))
                        .operation_timeout(Duration::from_secs(90))
                        .build(),
                )
                .build(),
        ))
    }
}

#[derive(Clone, Debug)]
pub struct Backup {
    pub key: String,
    pub size: u64,
    pub modified: DateTime,
    pub etag: Option<String>,
}

impl Backup {
    pub fn file_name(&self) -> &str {
        // Object keys always use '/', independently of the host OS.
        self.key.rsplit('/').next().unwrap_or(&self.key)
    }
}

fn consider_latest(latest: &mut Option<Backup>, object: &Object, prefix: &str) {
    let (Some(key), Some(modified), Some(size)) =
        (object.key(), object.last_modified(), object.size())
    else {
        return;
    };
    if !key.starts_with(prefix) || key.ends_with('/') || size < 0 {
        return;
    }
    let candidate = Backup {
        key: key.into(),
        size: size as u64,
        modified: *modified,
        etag: object.e_tag().map(str::to_owned),
    };
    if latest
        .as_ref()
        .is_none_or(|old| (candidate.modified, &candidate.key) > (old.modified, &old.key))
    {
        *latest = Some(candidate);
    }
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        detail.push_str(": ");
        detail.push_str(&error.to_string());
        source = error.source();
    }
    detail
}

pub async fn latest_backup(client: &Client, bucket: &str, prefix: &str) -> Result<Option<Backup>> {
    let mut pages = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .into_paginator()
        .send();
    let mut latest = None;
    let mut page_number = 0u64;
    tracing::info!(target:"app::s3",%prefix,"Looking for latest backup");
    while let Some(page) = pages.next().await {
        let page = page.map_err(|error| {
            // Report service diagnostics without formatting the signed HTTP request.
            let detail = error
                .as_service_error()
                .map(|e| {
                    format!(
                        "{}: {}",
                        e.meta().code().unwrap_or("S3 error"),
                        e.meta().message().unwrap_or("Request failed")
                    )
                })
                .unwrap_or_else(|| error_chain(&error));
            anyhow::anyhow!(
                "Could not list {bucket}/{prefix}: {detail}. Check the connection in Settings → Storage."
            )
        })?;
        page_number += 1;
        tracing::debug!(target:"app::s3",%prefix,page=page_number,objects=page.contents().len(),"S3 page received");
        for object in page.contents() {
            consider_latest(&mut latest, object, prefix);
        }
    }
    Ok(latest)
}

#[derive(Debug)]
pub enum DownloadError {
    Cancelled,
    Failed(String),
}

static DOWNLOAD_ID: AtomicU64 = AtomicU64::new(0);

pub async fn download(
    client: &Client,
    bucket: &str,
    backup: &Backup,
    destination: &Path,
    mut cancel: watch::Receiver<bool>,
    mut progress: impl FnMut(u64) + Send,
) -> std::result::Result<PathBuf, DownloadError> {
    let name = destination
        .file_name()
        .ok_or_else(|| DownloadError::Failed("Choose a file name".into()))?;
    let mut temporary_name = name.to_os_string();
    temporary_name.push(format!(
        ".{}.{}.part",
        std::process::id(),
        DOWNLOAD_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = destination.with_file_name(temporary_name);

    if *cancel.borrow() {
        return Err(DownloadError::Cancelled);
    }
    let mut temporary_created = false;
    let transfer = async {
        if fs::try_exists(destination).await? {
            bail!("The download destination already exists.");
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .context("Could not create the download file")?;
        temporary_created = true;
        let response = client
            .get_object()
            .bucket(bucket)
            .key(&backup.key)
            .set_if_match(backup.etag.clone())
            .send()
            .await
            .map_err(|error| {
                let detail = error
                    .as_service_error()
                    .map(|e| {
                        format!(
                            "{}: {}",
                            e.meta().code().unwrap_or("S3 error"),
                            e.meta().message().unwrap_or("Request failed")
                        )
                    })
                    .unwrap_or_else(|| error_chain(&error));
                anyhow::anyhow!("Could not download the backup: {detail}")
            })?;
        if response
            .content_length()
            .is_some_and(|length| length < 0 || length as u64 != backup.size)
        {
            bail!("The backup changed since the list was loaded. Refresh and try again.");
        }
        let mut body = response.body;
        let mut downloaded = 0;
        let mut last_progress = Instant::now();
        while let Some(chunk) = body.try_next().await.context("Download interrupted")? {
            file.write_all(&chunk)
                .await
                .context("Could not write the backup (check free disk space)")?;
            downloaded += chunk.len() as u64;
            if last_progress.elapsed() >= Duration::from_secs(1) {
                progress(downloaded);
                last_progress = Instant::now();
            }
        }
        if downloaded != backup.size {
            bail!(
                "Incomplete download: expected {} bytes, received {downloaded}",
                backup.size
            );
        }
        file.flush().await?;
        file.sync_all().await?;
        progress(downloaded);
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::select! {
        biased;
        _ = cancel.changed() => Err(DownloadError::Cancelled),
        result = transfer => result.map_err(|error| DownloadError::Failed(format!("{error:#}"))),
    };
    if let Err(error) = result {
        if temporary_created {
            let _ = fs::remove_file(&temporary).await;
        }
        return Err(error);
    }
    if *cancel.borrow() {
        let _ = fs::remove_file(&temporary).await;
        return Err(DownloadError::Cancelled);
    }
    // Both names are on the same filesystem. Hard-link publication is atomic and
    // refuses to overwrite any file created at the destination during transfer.
    let published = fs::hard_link(&temporary, destination).await;
    let _ = fs::remove_file(&temporary).await;
    published.map_err(|error| {
        DownloadError::Failed(format!("Could not publish the downloaded backup: {error}."))
    })?;
    Ok(destination.to_owned())
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::env;

    fn object(key: &str, seconds: i64) -> Object {
        Object::builder()
            .key(key)
            .size(3)
            .last_modified(DateTime::from_secs(seconds))
            .build()
    }

    #[test]
    fn latest_uses_time_not_name_and_ignores_folders_or_incomplete_metadata() {
        let mut latest = None;
        for object in [
            object("ch/z-old", 10),
            object("ch/a-new", 20),
            object("ch/", 99),
            object("mysql/new", 100),
            Object::builder().key("ch/no-date").size(5).build(),
        ] {
            consider_latest(&mut latest, &object, "ch/");
        }
        assert_eq!(latest.unwrap().key, "ch/a-new");
    }

    #[test]
    fn equal_timestamps_have_a_deterministic_choice() {
        for keys in [["ch/a", "ch/b"], ["ch/b", "ch/a"]] {
            let mut latest = None;
            for key in keys {
                consider_latest(&mut latest, &object(key, 10), "ch/");
            }
            assert_eq!(latest.unwrap().key, "ch/b");
        }
    }

    fn test_client(endpoint: String) -> Client {
        Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .endpoint_url(endpoint)
                .force_path_style(true)
                .region(Region::new("us-east-1"))
                .credentials_provider(aws_sdk_s3::config::Credentials::new(
                    "test", "test", None, None, "test",
                ))
                .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
                .build(),
        )
    }

    fn serve(responses: Vec<String>) -> (Client, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = test_client(format!("http://{}", listener.local_addr().unwrap()));
        let thread = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (client, thread)
    }

    #[tokio::test]
    async fn newest_object_can_be_on_a_later_page() -> Result<()> {
        let pages = [
            ("z-old", "2026-01-01T00:00:00Z", true),
            ("a-new", "2026-02-01T00:00:00Z", false),
        ];
        let responses = pages.into_iter().map(|(key, date, more)| {
            let xml = format!("<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsTruncated>{more}</IsTruncated><NextContinuationToken>next</NextContinuationToken><Contents><Key>ch/{key}</Key><LastModified>{date}</LastModified><Size>3</Size></Contents></ListBucketResult>");
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}", xml.len())
        }).collect();
        let (client, thread) = serve(responses);
        assert_eq!(
            latest_backup(&client, "bucket", "ch/").await?.unwrap().key,
            "ch/a-new"
        );
        thread.join().unwrap();
        Ok(())
    }

    #[tokio::test]
    async fn streamed_download_is_complete_and_never_overwrites() -> Result<()> {
        let (client, thread) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".into(),
        ]);
        let directory = env::temp_dir().join(format!("sandbox-download-{}", std::process::id()));
        fs::create_dir_all(&directory).await?;
        let destination = directory.join("backup.sql");
        let backup = Backup {
            key: "mysql/backup.sql".into(),
            size: 3,
            modified: DateTime::from_secs(0),
            etag: None,
        };
        let (cancel, receiver) = watch::channel(false);
        let result = download(&client, "bucket", &backup, &destination, receiver, |_| {}).await;
        assert!(result.is_ok(), "{result:?}");
        thread.join().unwrap();
        assert_eq!(fs::read(&destination).await?, b"abc");
        let result = download(
            &client,
            "bucket",
            &backup,
            &destination,
            cancel.subscribe(),
            |_| {},
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Failed(_))));
        assert_eq!(fs::read(&destination).await?, b"abc");
        cancel.send_replace(true);
        assert!(matches!(
            download(
                &client,
                "bucket",
                &backup,
                &directory.join("cancelled.sql"),
                cancel.subscribe(),
                |_| {}
            )
            .await,
            Err(DownloadError::Cancelled)
        ));
        assert_eq!(std::fs::read_dir(&directory)?.count(), 1);
        fs::remove_dir_all(directory).await?;
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_response_removes_the_partial_file() -> Result<()> {
        let (client, thread) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabc".into(),
        ]);
        let directory = env::temp_dir().join(format!("sandbox-truncated-{}", std::process::id()));
        fs::create_dir_all(&directory).await?;
        let destination = directory.join("backup.sql");
        let backup = Backup {
            key: "mysql/backup.sql".into(),
            size: 8,
            modified: DateTime::from_secs(0),
            etag: None,
        };
        let (_cancel, receiver) = watch::channel(false);
        let result = download(&client, "bucket", &backup, &destination, receiver, |_| {}).await;
        assert!(matches!(result, Err(DownloadError::Failed(_))));
        thread.join().unwrap();
        assert_eq!(std::fs::read_dir(&directory)?.count(), 0);
        fs::remove_dir_all(directory).await?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_a_stalled_download_removes_the_partial_file() -> Result<()> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let client = test_client(format!("http://{}", listener.local_addr()?));
        let (ready, receiving) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\na")
                .unwrap();
            let _ = ready.send(());
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        let directory = env::temp_dir().join(format!("sandbox-cancel-{}", std::process::id()));
        fs::create_dir_all(&directory).await?;
        let destination = directory.join("backup.sql");
        let backup = Backup {
            key: "mysql/backup.sql".into(),
            size: 8,
            modified: DateTime::from_secs(0),
            etag: None,
        };
        let (cancel, receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            download(&client, "bucket", &backup, &destination, receiver, |_| {}).await
        });
        tokio::time::timeout(Duration::from_secs(5), receiving).await??;
        cancel.send(true)?;
        let result = tokio::time::timeout(Duration::from_secs(2), task).await??;
        let _ = release.send(());
        thread.join().unwrap();
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert_eq!(std::fs::read_dir(&directory)?.count(), 0);
        fs::remove_dir_all(directory).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "Requires saved application credentials and network access; reads object metadata only"]
    async fn live_rustfs_lists_both_prefixes() -> Result<()> {
        let config = crate::database::load_storage()?.unwrap_or_default();
        let client = config.client().await?;
        for prefix in ["ch/", "mysql/"] {
            let backup = latest_backup(&client, &config.bucket, prefix).await?;
            println!("{prefix}: {backup:?}");
        }
        Ok(())
    }
}
