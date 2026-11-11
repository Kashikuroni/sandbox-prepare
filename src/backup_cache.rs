use crate::{
    archive_check,
    backups::{self, Backup, DownloadError, StorageConfig},
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

pub const SOURCES: [(&str, &str); 2] = [("ClickHouse", "ch/"), ("MySQL", "mysql/")];
pub const FILES: [&str; 2] = ["clickhouse.tar.zst", "mysql.xbstream.zst"];
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    endpoint: String,
    bucket: String,
}
impl Origin {
    pub fn new(config: &StorageConfig) -> Self {
        Self {
            endpoint: config.endpoint.trim_end_matches('/').into(),
            bucket: config.bucket.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub size: u64,
    pub modified: String,
    pub etag: Option<String>,
    pub sha256: String,
    #[serde(default)]
    pub local_modified: Option<FileTime>,
    #[serde(default)]
    pub verification_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileTime {
    seconds: u64,
    nanos: u32,
}
fn file_time(path: &Path, expected_size: u64) -> Result<FileTime> {
    let metadata = fs::symlink_metadata(path).context("Local file is missing")?;
    ensure!(
        metadata.file_type().is_file(),
        "Local copy is not a regular file"
    );
    ensure!(metadata.len() == expected_size, "Local file size changed");
    let modified = metadata.modified()?.duration_since(UNIX_EPOCH)?;
    Ok(FileTime {
        seconds: modified.as_secs(),
        nanos: modified.subsec_nanos(),
    })
}
impl Record {
    pub fn matches(&self, remote: &Backup) -> bool {
        self.key == remote.key
            && self.size == remote.size
            && self.modified == remote.modified.to_string()
            && self.etag == remote.etag
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pair {
    pub generation: String,
    pub origin: Origin,
    pub records: [Record; 2],
}
#[derive(Clone, Default)]
pub struct Snapshot {
    pub pair: Option<Pair>,
    pub valid: [bool; 2],
    pub warnings: Vec<String>,
}
impl Snapshot {
    pub fn missing(&self) -> bool {
        self.pair.is_none() || self.valid.contains(&false)
    }
    pub fn current(&self, config: &StorageConfig, remote: &[Backup; 2]) -> bool {
        !self.missing()
            && self.pair.as_ref().is_some_and(|pair| {
                pair.origin == Origin::new(config)
                    && pair
                        .records
                        .iter()
                        .zip(remote)
                        .all(|(local, remote)| local.matches(remote))
            })
    }
}
#[derive(Clone)]
pub struct Cache {
    root: PathBuf,
    database: PathBuf,
}
impl Cache {
    pub fn application() -> Result<Self> {
        let database = crate::database::database_path()?;
        let root = database
            .parent()
            .context("Application directory is missing")?
            .join("backups");
        Ok(Self { root, database })
    }
    fn connection(&self) -> Result<Connection> {
        let db = Connection::open(&self.database)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS backup_pair (id INTEGER PRIMARY KEY CHECK(id=1), manifest TEXT NOT NULL);")?;
        Ok(db)
    }
    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".lock"))?;
        file.try_lock()
            .context("Backups are busy in another application instance")?;
        Ok(file)
    }
    fn read_pair(&self) -> Result<Option<Pair>> {
        let json: Option<String> = self
            .connection()?
            .query_row("SELECT manifest FROM backup_pair WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let pair: Option<Pair> = json
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .context("Invalid local backup manifest")?;
        if let Some(pair) = &pair {
            ensure!(
                owned_name(&pair.generation, "pair-"),
                "Invalid backup generation"
            );
        }
        Ok(pair)
    }
    fn store_pair(&self, pair: &Pair) -> Result<()> {
        self.connection()?.execute("INSERT INTO backup_pair(id,manifest) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET manifest=excluded.manifest", params![serde_json::to_string(pair)?])?;
        Ok(())
    }
    pub fn load(&self) -> Result<Snapshot> {
        let _lock = self.lock()?;
        let mut pair = self.read_pair()?;
        let mut valid = [false; 2];
        let mut migrated = false;
        if let Some(pair) = &mut pair {
            let directory = self.root.join(&pair.generation);
            if fs::symlink_metadata(&directory).is_ok_and(|m| m.file_type().is_dir()) {
                for (index, record) in pair.records.iter_mut().enumerate() {
                    if let Ok(modified) = file_time(&directory.join(FILES[index]), record.size) {
                        // Older manifests already hold a digest from the original full check.
                        // Record a stat baseline once, without rereading gigabytes on upgrade.
                        if record.local_modified.is_none() && record.verification_error.is_none() {
                            record.local_modified = Some(modified.clone());
                            migrated = true;
                        }
                        valid[index] = record.local_modified.as_ref() == Some(&modified)
                            && record.verification_error.is_none();
                    }
                }
            }
            if migrated {
                self.store_pair(pair)?;
            }
        }
        let warnings = self.cleanup(pair.as_ref().map(|pair| pair.generation.as_str()));
        Ok(Snapshot {
            pair,
            valid,
            warnings,
        })
    }
    fn cleanup(&self, keep: Option<&str>) -> Vec<String> {
        let mut warnings = Vec::new();
        let result = (|| -> Result<()> {
            for entry in fs::read_dir(&self.root)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if Some(name.as_str()) != keep
                    && (owned_name(&name, "pair-") || owned_name(&name, ".pending-"))
                    && entry.file_type()?.is_dir()
                    && let Err(error) = fs::remove_dir_all(entry.path())
                {
                    warnings.push(format!("Could not remove old backup set {name}: {error}"));
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            warnings.push(format!("Backup cleanup failed: {error:#}"));
        }
        warnings
    }
    fn begin(&self) -> Result<Pending> {
        let lock = self.lock()?;
        self.read_pair()?; // A corrupt pointer must never trigger destructive cleanup.
        let id = format!(
            "{}-{}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let directory = self.root.join(format!(".pending-{id}"));
        fs::create_dir(&directory)?;
        Ok(Pending {
            cache: self.clone(),
            directory,
            generation: format!("pair-{id}"),
            _lock: lock,
            published: false,
        })
    }
}
fn owned_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        let parts: Vec<_> = suffix.split('-').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    })
}
struct Pending {
    cache: Cache,
    directory: PathBuf,
    generation: String,
    _lock: File,
    published: bool,
}
impl Pending {
    fn commit(mut self, pair: Pair) -> Result<Snapshot> {
        // Durable files first, then a single SQLite commit publishes both paths together.
        for name in FILES {
            File::open(self.directory.join(name))?.sync_all()?;
        }
        sync_directory(&self.directory)?;
        let ready = self.cache.root.join(&self.generation);
        fs::rename(&self.directory, &ready)?;
        self.directory = ready;
        sync_directory(&self.cache.root)?;
        self.cache.store_pair(&pair)?;
        self.published = true;
        let warnings = self.cache.cleanup(Some(&pair.generation));
        Ok(Snapshot {
            pair: Some(pair),
            valid: [true; 2],
            warnings,
        })
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        if self
            .directory
            .file_name()
            .is_some_and(|name| name == self.generation.as_str())
        {
            match self.cache.read_pair() {
                Ok(Some(pair)) if pair.generation == self.generation => return,
                Err(error) => {
                    tracing::warn!(target:"app::storage",%error,"Retaining backup files because publication state is uncertain");
                    return;
                }
                _ => {}
            }
        }
        if !self.published
            && let Err(error) = fs::remove_dir_all(&self.directory)
        {
            tracing::warn!(target:"app::storage",%error,"Could not remove incomplete backup set");
        }
    }
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Clone)]
pub struct UpdateControl {
    phase: std::sync::Arc<std::sync::atomic::AtomicU8>,
    sender: watch::Sender<bool>,
}
impl Default for UpdateControl {
    fn default() -> Self {
        Self::new()
    }
}
impl UpdateControl {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self {
            phase: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            sender,
        }
    }
    pub fn cancel(&self) -> bool {
        if self
            .phase
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.sender.send_replace(true);
            true
        } else {
            false
        }
    }
    fn cancelled(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == 1
    }
    fn begin_publish(&self) -> Result<()> {
        self.phase
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| anyhow::anyhow!("Backup update cancelled"))?;
        Ok(())
    }
}

pub enum Progress {
    Started(usize),
    Bytes(usize, u64),
    Verifying(usize),
    VerificationBytes(usize, u64, u64),
    VerificationFailed(usize),
    Verified(usize),
    Publishing,
}
pub async fn update(
    cache: Cache,
    config: StorageConfig,
    remote: [Backup; 2],
    control: UpdateControl,
    mut progress: impl FnMut(Progress) + Send,
) -> std::result::Result<Snapshot, DownloadError> {
    let cancel = control.sender.subscribe();
    let result = async {
        let pending = tokio::task::spawn_blocking(move || cache.begin()).await??;
        let client = config.client().await?;
        let mut records = Vec::new();
        for (index, backup) in remote.iter().enumerate() {
            anyhow::ensure!(!*cancel.borrow(), "Cancelled");
            progress(Progress::Started(index));
            let path = pending.directory.join(FILES[index]);
            backups::download(
                &client,
                &config.bucket,
                backup,
                &path,
                cancel.clone(),
                |bytes| progress(Progress::Bytes(index, bytes)),
            )
            .await
            .map_err(|error| match error {
                DownloadError::Cancelled => anyhow::anyhow!("Cancelled"),
                DownloadError::Failed(error) => anyhow::anyhow!(error),
            })?;
            progress(Progress::Verifying(index));
            let check_cancel = cancel.clone();
            let size = backup.size;
            let (verification_events, mut verification_receiver) =
                tokio::sync::mpsc::unbounded_channel();
            // Forward throttled verification updates while the blocking worker runs.
            let verification =
                tokio::task::spawn_blocking(move || -> Result<(String, FileTime)> {
                    let before = file_time(&path, size)?;
                    let digest = archive_check::verify_with_progress(
                        &path,
                        index,
                        &check_cancel,
                        |bytes| {
                            let _ = verification_events
                                .send(Progress::VerificationBytes(index, bytes, size));
                        },
                    )?;
                    let after = file_time(&path, size)?;
                    ensure!(before == after, "File changed during verification");
                    Ok((digest, after))
                });
            while let Some(event) = verification_receiver.recv().await {
                progress(event);
            }
            let (digest, local_modified) = verification.await??;
            records.push(Record {
                key: backup.key.clone(),
                size: backup.size,
                modified: backup.modified.to_string(),
                etag: backup.etag.clone(),
                sha256: digest,
                local_modified: Some(local_modified),
                verification_error: None,
            });
            progress(Progress::Verified(index));
        }
        anyhow::ensure!(!*cancel.borrow(), "Cancelled");
        let pair = Pair {
            generation: pending.generation.clone(),
            origin: Origin::new(&config),
            records: records
                .try_into()
                .map_err(|_| anyhow::anyhow!("Incomplete backup pair"))?,
        };
        control.begin_publish()?;
        progress(Progress::Publishing);
        tokio::task::spawn_blocking(move || pending.commit(pair)).await?
    }
    .await;
    result.map_err(|error: anyhow::Error| {
        if control.cancelled() {
            DownloadError::Cancelled
        } else {
            DownloadError::Failed(format!("{error:#}"))
        }
    })
}

// Read-only archive checks; only verification metadata is updated after completion.
pub fn verify_local(
    cache: Cache,
    control: UpdateControl,
    mut progress: impl FnMut(Progress),
) -> std::result::Result<Snapshot, DownloadError> {
    let cancel = control.sender.subscribe();
    let result = (|| -> Result<Snapshot> {
        let _lock = cache.lock()?;
        let mut pair = cache.read_pair()?.context("No local backups to verify")?;
        let directory = cache.root.join(&pair.generation);
        ensure!(
            fs::symlink_metadata(&directory)?.file_type().is_dir(),
            "Local backup directory is missing or invalid"
        );
        let mut valid = [false; 2];
        for (index, record) in pair.records.iter_mut().enumerate() {
            ensure!(!*cancel.borrow(), "Verification cancelled");
            progress(Progress::Verifying(index));
            let path = directory.join(FILES[index]);
            let check = (|| -> Result<FileTime> {
                let before = file_time(&path, record.size)?;
                let digest = archive_check::verify_with_progress(&path, index, &cancel, |bytes| {
                    progress(Progress::VerificationBytes(index, bytes, record.size));
                })?;
                ensure!(
                    digest == record.sha256,
                    "SHA-256 differs from the downloaded backup"
                );
                let after = file_time(&path, record.size)?;
                ensure!(before == after, "File changed during verification");
                Ok(after)
            })();
            ensure!(!*cancel.borrow(), "Verification cancelled");
            match check {
                Ok(modified) => {
                    valid[index] = true;
                    record.local_modified = Some(modified);
                    record.verification_error = None;
                    progress(Progress::Verified(index));
                }
                Err(error) => {
                    record.verification_error = Some(format!("{error:#}"));
                    progress(Progress::VerificationFailed(index));
                }
            }
        }
        control.begin_publish()?;
        progress(Progress::Publishing);
        cache.store_pair(&pair)?;
        Ok(Snapshot {
            pair: Some(pair),
            valid,
            warnings: Vec::new(),
        })
    })();
    result.map_err(|error| {
        if control.cancelled() {
            DownloadError::Cancelled
        } else {
            DownloadError::Failed(format!("{error:#}"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_check::fixtures;
    fn cache(label: &str) -> Cache {
        let directory = std::env::temp_dir().join(format!(
            "sandbox-cache-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        Cache {
            root: directory.join("backups"),
            database: directory.join("application.sqlite3"),
        }
    }
    fn install(cache: &Cache, config: &StorageConfig) -> Result<Snapshot> {
        let pending = cache.begin()?;
        let mut records = Vec::new();
        let (_, cancel) = watch::channel(false);
        for (index, name) in FILES.iter().enumerate() {
            let bytes = fixtures::compress(&if index == 0 {
                fixtures::tar_bytes()
            } else {
                fixtures::xb_bytes()
            });
            let path = pending.directory.join(name);
            fs::write(&path, &bytes)?;
            records.push(Record {
                key: format!("{}/old", SOURCES[index].1),
                size: bytes.len() as u64,
                modified: "2026-01-01T00:00:00Z".into(),
                etag: Some("old".into()),
                sha256: archive_check::verify(&path, index, &cancel)?,
                local_modified: Some(file_time(&path, bytes.len() as u64)?),
                verification_error: None,
            });
        }
        let pair = Pair {
            generation: pending.generation.clone(),
            origin: Origin::new(config),
            records: records.try_into().unwrap(),
        };
        pending.commit(pair)
    }
    #[test]
    fn abandoned_updates_recover_and_corrupt_local_files_require_download() -> Result<()> {
        let cache = cache("recovery");
        let config = StorageConfig::default();
        let old = install(&cache, &config)?.pair.unwrap();
        {
            let pending = cache.begin()?;
            fs::write(pending.directory.join(FILES[0]), b"partial")?;
        }
        assert_eq!(cache.load()?.pair.unwrap().generation, old.generation);
        let stale = cache.root.join(".pending-123-4-5");
        fs::create_dir(&stale)?;
        assert!(!cache.load()?.missing());
        assert!(!stale.exists());
        let path = cache.root.join(&old.generation).join(FILES[1]);
        let mut data = fs::read(&path)?;
        data[5] ^= 1;
        fs::write(&path, data)?;
        let snapshot = cache.load()?;
        assert!(snapshot.valid[0]);
        assert!(!snapshot.valid[1]);
        assert!(snapshot.missing());
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }

    #[test]
    fn quick_check_uses_metadata_and_explicit_verification_persists_corruption() -> Result<()> {
        let cache = cache("quick-check");
        let pair = install(&cache, &StorageConfig::default())?.pair.unwrap();
        let path = cache.root.join(&pair.generation).join(FILES[0]);
        let modified = fs::metadata(&path)?.modified()?;
        let mut bytes = fs::read(&path)?;
        bytes[0] ^= 1;
        fs::write(&path, bytes)?;
        // Same size and mtime intentionally evade stat checks: load must not hash.
        File::open(&path)?.set_times(fs::FileTimes::new().set_modified(modified))?;
        assert!(!cache.load()?.missing());
        let mut checked = Vec::new();
        let snapshot = verify_local(cache.clone(), UpdateControl::new(), |event| {
            if let Progress::Verifying(index) = event {
                checked.push(index);
            }
        })
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(checked, vec![0, 1]);
        assert_eq!(snapshot.valid, [false, true]);
        assert!(
            snapshot.pair.unwrap().records[0]
                .verification_error
                .is_some()
        );
        assert_eq!(cache.load()?.valid, [false, true]);
        assert!(path.exists()); // Verification never removes local files.
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }

    #[test]
    fn changed_mtime_requires_verification_and_success_refreshes_the_baseline() -> Result<()> {
        let cache = cache("mtime");
        let pair = install(&cache, &StorageConfig::default())?.pair.unwrap();
        let path = cache.root.join(&pair.generation).join(FILES[1]);
        let modified = fs::metadata(&path)?.modified()? + std::time::Duration::from_secs(2);
        File::open(&path)?.set_times(fs::FileTimes::new().set_modified(modified))?;
        assert_eq!(cache.load()?.valid, [true, false]);
        let mut progress = [0; 2];
        let snapshot = verify_local(cache.clone(), UpdateControl::new(), |event| {
            if let Progress::VerificationBytes(index, bytes, total) = event {
                assert!(bytes <= total);
                assert!(bytes >= progress[index]);
                progress[index] = bytes;
            }
        })
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert!(!snapshot.missing());
        assert_eq!(progress, pair.records.map(|record| record.size));
        assert!(!cache.load()?.missing());
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }

    #[test]
    fn legacy_manifest_gains_a_metadata_baseline_without_losing_source_identity() -> Result<()> {
        let cache = cache("legacy-metadata");
        let pair = install(&cache, &StorageConfig::default())?.pair.unwrap();
        let mut legacy = serde_json::to_value(&pair)?;
        for record in legacy["records"].as_array_mut().unwrap() {
            record.as_object_mut().unwrap().remove("local_modified");
            record.as_object_mut().unwrap().remove("verification_error");
        }
        cache.connection()?.execute(
            "UPDATE backup_pair SET manifest=?1",
            params![legacy.to_string()],
        )?;
        let snapshot = cache.load()?;
        assert!(!snapshot.missing());
        let migrated = cache.read_pair()?.unwrap();
        assert_eq!(migrated.generation, pair.generation);
        for (index, record) in migrated.records.iter().enumerate() {
            assert!(record.local_modified.is_some());
            assert_eq!(record.sha256, pair.records[index].sha256);
            assert_eq!(record.key, pair.records[index].key);
        }
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }

    #[test]
    fn cancelled_manual_verification_preserves_manifest_and_files() -> Result<()> {
        let cache = cache("verify-cancel");
        let pair = install(&cache, &StorageConfig::default())?.pair.unwrap();
        let before = serde_json::to_value(&cache.read_pair()?)?;
        let control = UpdateControl::new();
        let action = control.clone();
        let result = verify_local(cache.clone(), control, |event| {
            if matches!(event, Progress::Verified(0)) {
                assert!(action.cancel());
            }
        });
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert_eq!(serde_json::to_value(cache.read_pair()?)?, before);
        assert!(!cache.load()?.missing());
        assert!(cache.root.join(pair.generation).join(FILES[0]).exists());
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }
    #[test]
    fn lock_and_invalid_manifest_never_delete_current_files() -> Result<()> {
        let cache = cache("lock");
        let old = install(&cache, &StorageConfig::default())?.pair.unwrap();
        {
            let _pending = cache.begin()?;
            assert!(cache.load().is_err());
        }
        cache
            .connection()?
            .execute("UPDATE backup_pair SET manifest='invalid'", [])?;
        assert!(cache.load().is_err());
        assert!(cache.root.join(old.generation).exists());
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }
    #[test]
    fn sqlite_failure_preserves_the_previous_generation() -> Result<()> {
        let cache = cache("sqlite-failure");
        let config = StorageConfig::default();
        let old = install(&cache, &config)?.pair.unwrap();
        cache.connection()?.execute_batch("CREATE TRIGGER fail_publish BEFORE UPDATE ON backup_pair BEGIN SELECT RAISE(ABORT, 'simulated failure'); END;")?;
        assert!(install(&cache, &config).is_err());
        let snapshot = cache.load()?;
        assert!(!snapshot.missing());
        assert_eq!(snapshot.pair.unwrap().generation, old.generation);
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }
    #[test]
    fn cancellation_and_publication_have_one_atomic_winner() {
        for _ in 0..32 {
            let control = UpdateControl::new();
            let other = control.clone();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let ready = barrier.clone();
            let thread = std::thread::spawn(move || {
                ready.wait();
                other.cancel()
            });
            barrier.wait();
            let published = control.begin_publish().is_ok();
            let cancelled = thread.join().unwrap();
            assert_ne!(published, cancelled);
            assert_eq!(control.cancelled(), cancelled);
        }
        let control = UpdateControl::new();
        assert!(control.cancel());
        assert!(control.begin_publish().is_err());
        let control = UpdateControl::new();
        assert!(control.begin_publish().is_ok());
        assert!(!control.cancel());
    }
    #[test]
    fn freshness_compares_both_records_and_storage_origin() -> Result<()> {
        let cache = cache("freshness");
        let config = StorageConfig::default();
        let mut snapshot = install(&cache, &config)?;
        let pair = snapshot.pair.as_mut().unwrap();
        let remote = std::array::from_fn(|index| {
            let modified = aws_sdk_s3::primitives::DateTime::from_secs(1);
            pair.records[index].modified = modified.to_string();
            Backup {
                key: pair.records[index].key.clone(),
                size: pair.records[index].size,
                modified,
                etag: pair.records[index].etag.clone(),
            }
        });
        assert!(snapshot.current(&config, &remote));
        let mut changed = remote.clone();
        changed[1].etag = Some("new".into());
        assert!(!snapshot.current(&config, &changed));
        let mut other_source = config.clone();
        other_source.bucket = "other".into();
        assert!(!snapshot.current(&other_source, &remote));
        snapshot.valid[0] = false;
        assert!(!snapshot.current(&config, &remote));
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }
    fn server(bodies: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    bytes.push(byte[0]);
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (format!("http://{addr}"), task)
    }
    #[tokio::test]
    async fn accepted_cancel_after_first_verified_file_preserves_old_pair() -> Result<()> {
        let cache = cache("cancel");
        let bodies = vec![fixtures::compress(&fixtures::tar_bytes())];
        let (endpoint, server) = server(bodies.clone());
        let config = StorageConfig {
            endpoint,
            bucket: "test".into(),
            region: "test".into(),
            access_key: "test".into(),
            secret_key: "test".into(),
        };
        let old = install(&cache, &config)?.pair.unwrap();
        let remote = std::array::from_fn(|index| Backup {
            key: format!("{}new", SOURCES[index].1),
            size: bodies[0].len() as u64,
            modified: aws_sdk_s3::primitives::DateTime::from_secs(10),
            etag: None,
        });
        let control = UpdateControl::new();
        let action = control.clone();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            update(cache.clone(), config, remote, control, |progress| {
                if matches!(progress, Progress::Verified(0)) {
                    assert!(action.cancel());
                }
            }),
        )
        .await?;
        server.join().unwrap();
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert_eq!(cache.load()?.pair.unwrap().generation, old.generation);
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }
    #[tokio::test]
    async fn failed_pair_preserves_old_and_success_replaces_both_then_cleans_old() -> Result<()> {
        let cache = cache("update");
        for valid in [false, true] {
            let bodies = vec![
                fixtures::compress(&fixtures::tar_bytes()),
                if valid {
                    fixtures::compress(&fixtures::xb_bytes())
                } else {
                    fixtures::compress(b"invalid mysql")
                },
            ];
            let (endpoint, server) = server(bodies.clone());
            let config = StorageConfig {
                endpoint,
                bucket: "test".into(),
                region: "test".into(),
                access_key: "test".into(),
                secret_key: "test".into(),
            };
            let before = if cache.load()?.pair.is_none() {
                install(&cache, &config)?
            } else {
                cache.load()?
            };
            let old = before.pair.unwrap();
            let remote = std::array::from_fn(|index| Backup {
                key: format!("{}new", SOURCES[index].1),
                size: bodies[index].len() as u64,
                modified: aws_sdk_s3::primitives::DateTime::from_secs(10),
                etag: None,
            });
            let control = UpdateControl::new();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                update(
                    cache.clone(),
                    config.clone(),
                    remote.clone(),
                    control,
                    |_| {},
                ),
            )
            .await
            .context("Mock S3 test timed out")?;
            server.join().unwrap();
            if valid {
                let snapshot = result.map_err(|e| anyhow::anyhow!("{e:?}"))?;
                assert!(snapshot.current(&config, &remote));
                assert!(!cache.root.join(&old.generation).exists());
                assert_eq!(
                    cache.load()?.pair.unwrap().generation,
                    snapshot.pair.unwrap().generation
                );
            } else {
                assert!(result.is_err());
                assert_eq!(cache.load()?.pair.unwrap().generation, old.generation);
            }
        }
        fs::remove_dir_all(cache.database.parent().unwrap())?;
        Ok(())
    }
}
