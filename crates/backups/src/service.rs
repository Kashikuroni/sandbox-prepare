//! Project-owned backup operations. UI subscriptions do not own the work.
use crate::{
    backup_cache::{self, Cache, Progress, Snapshot, UpdateControl},
    backups::{self, Backup, DownloadError, StorageClient},
};
use anyhow::{Context, Result, ensure};
use sandbox_core::{
    StorageConfig,
    models::{DatabaseEngine, Project, ProjectId, Scope},
    operations::{Operation, Operations},
};
use sandbox_platform::catalog::Catalog;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

pub struct Service {
    catalog: Catalog,
    project: Mutex<Project>,
    pub cache: Cache,
    operations: Arc<Operations>,
    activity: Arc<AtomicUsize>,
    pub revision: AtomicU64,
    update: Mutex<Option<UpdateControl>>,
    preparation: Mutex<()>,
}
struct Activity(Arc<AtomicUsize>);
impl Drop for Activity {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Updating(Arc<Service>);
impl Drop for Updating {
    fn drop(&mut self) {
        *self.0.update.lock().unwrap() = None;
    }
}

impl Service {
    pub fn new(catalog: Catalog, id: ProjectId, operations: Arc<Operations>) -> Result<Arc<Self>> {
        let project = catalog.project(&id)?;
        crate::logging::register_credentials(
            &project.storage.access_key,
            &project.storage.secret_key,
        );
        Ok(Arc::new(Self {
            cache: Cache::for_project(catalog.clone(), id)?,
            catalog,
            project: Mutex::new(project),
            operations,
            activity: Arc::new(AtomicUsize::new(0)),
            revision: AtomicU64::new(0),
            update: Mutex::new(None),
            preparation: Mutex::new(()),
        }))
    }
    pub fn project(&self) -> Project {
        self.project.lock().unwrap().clone()
    }
    fn reserve(&self) -> (Project, Activity) {
        let project = self.project.lock().unwrap();
        self.activity.fetch_add(1, Ordering::AcqRel);
        (project.clone(), Activity(self.activity.clone()))
    }
    pub fn busy(&self) -> bool {
        self.activity.load(Ordering::Acquire) > 0
    }
    pub fn configure(&self, config: StorageConfig) -> Result<()> {
        let mut project = self.project.lock().unwrap();
        ensure!(
            !self.busy(),
            "Wait for the current backup operations before changing the connection"
        );
        ensure!(
            !config.access_key.is_empty()
                && !config.secret_key.is_empty()
                && !config.bucket.trim().is_empty()
                && !config.region.trim().is_empty(),
            "Enter the bucket, region and both keys"
        );
        ensure!(
            config.endpoint.starts_with("https://") || config.endpoint.starts_with("http://"),
            "Storage endpoint must use HTTP(S)"
        );
        crate::logging::register_credentials(&config.access_key, &config.secret_key);
        let mut next = project.clone();
        next.storage = config;
        self.catalog.save_project(&next)?;
        *project = next;
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }
    pub fn configure_prefixes(
        &self,
        databases: std::collections::BTreeMap<
            sandbox_core::models::DatabaseId,
            sandbox_core::models::DatabaseSpec,
        >,
    ) -> Result<()> {
        let mut project = self.project.lock().unwrap();
        ensure!(
            self.activity.load(Ordering::Acquire) == 0 && self.update.lock().unwrap().is_none(),
            "Wait for current backup operations before changing prefixes"
        );
        let mut next = project.clone();
        next.databases = databases;
        self.catalog.save_project(&next)?;
        *project = next;
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }
    fn scope(project: &Project) -> Scope {
        Scope {
            project: Some(project.id.clone()),
            ..Default::default()
        }
    }
    /// Shared project cache: serialize preparation, reuse manual updates, cancel only our own download.
    pub fn ensure_latest(
        self: &Arc<Self>,
        cancel: &std::sync::atomic::AtomicBool,
        progress: impl Fn(String, Option<f32>) + Send + Sync + 'static,
    ) -> Result<()> {
        use std::{
            sync::{TryLockError, mpsc},
            time::Duration,
        };
        let check = || -> Result<()> {
            ensure!(!cancel.load(Ordering::Acquire), "Cancelled");
            Ok(())
        };
        progress("Checking current backups…".into(), None);
        let _gate = loop {
            check()?;
            match self.preparation.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(100)),
                Err(TryLockError::Poisoned(_)) => {
                    anyhow::bail!("Backup preparation lock was poisoned")
                }
            }
        };
        let (project, _activity) = self.reserve();
        while self.update.lock().unwrap().is_some() {
            check()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        let prefix = |engine| {
            project
                .databases
                .values()
                .find(|db| db.engine == engine)
                .map(|db| db.prefix.clone())
                .context("Backup source is not configured")
        };
        let ch = prefix(DatabaseEngine::ClickHouse)?;
        let mysql = prefix(DatabaseEngine::MySql)?;
        let remote=self.operations.handle().block_on(async {
            let request=async {
                let client=project.storage.client().await?;
                let (ch,mysql)=tokio::try_join!(backups::latest_backup(&client,&project.storage.bucket,&ch),backups::latest_backup(&client,&project.storage.bucket,&mysql))?;
                Ok::<_,anyhow::Error>([ch.context("No ClickHouse backup found")?,mysql.context("No MySQL backup found")?])
            };
            tokio::select!{result=request=>result,_=async{while !cancel.load(Ordering::Acquire){tokio::time::sleep(Duration::from_millis(100)).await;}}=>Err(anyhow::anyhow!("Cancelled"))}
        })?;
        let totals = [remote[0].size, remote[1].size];
        let progress = Arc::new(progress);
        loop {
            check()?;
            if self.cache.load()?.current(&project.storage, &remote) {
                return Ok(());
            }
            if self.update.lock().unwrap().is_some() {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let (send, receive) = mpsc::channel();
            let notify = progress.clone();
            let bytes = Arc::new([
                std::sync::atomic::AtomicU64::new(0),
                std::sync::atomic::AtomicU64::new(0),
            ]);
            let download = self.download(
                remote.clone(),
                move |event| {
                    let title = |index| if index == 0 { "ClickHouse" } else { "MySQL" };
                    match event {
                        Progress::Started(i) => {
                            notify(format!("Downloading {} backup…", title(i)), None)
                        }
                        Progress::Bytes(i, count) => {
                            bytes[i].store(count, Ordering::Relaxed);
                            let done = bytes.iter().map(|n| n.load(Ordering::Relaxed)).sum::<u64>();
                            let total = totals.iter().sum::<u64>();
                            notify(
                                "Downloading backups…".into(),
                                (total > 0).then(|| (done as f32 / total as f32).min(1.)),
                            );
                        }
                        Progress::Verifying(i) => {
                            notify(format!("Verifying {} backup…", title(i)), None)
                        }
                        Progress::VerificationBytes(i, done, total) => notify(
                            format!("Verifying {} backup…", title(i)),
                            (total > 0).then(|| (done as f32 / total as f32).min(0.99)),
                        ),
                        Progress::Publishing => notify("Publishing verified backups…".into(), None),
                        _ => {}
                    }
                },
                move |result| {
                    let _ = send.send(result);
                },
            );
            let control = match download {
                Ok(control) => control,
                Err(error) => {
                    if self.update.lock().unwrap().is_some() {
                        continue;
                    }
                    return Err(error);
                }
            };
            loop {
                if cancel.load(Ordering::Acquire) {
                    control.cancel();
                }
                match receive.recv_timeout(Duration::from_millis(100)) {
                    Ok(result) => {
                        result.map_err(|error| anyhow::anyhow!("{error:?}"))?;
                        check()?;
                        return Ok(());
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    pub fn check_local(
        self: &Arc<Self>,
        finished: impl FnOnce(std::result::Result<Snapshot, String>) + Send + 'static,
    ) -> Result<()> {
        let (project, activity) = self.reserve();
        let cache = self.cache.clone();
        self.operations.spawn_blocking(
            Self::scope(&project),
            "Check local backups",
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            move |job| {
                let _activity = activity;
                job.running();
                let result = cache.load().map_err(|e| format!("{e:#}"));
                let outcome = result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("{e}"));
                finished(result);
                outcome
            },
        )?;
        Ok(())
    }
    pub fn check_remote(
        self: &Arc<Self>,
        index: usize,
        finished: impl FnOnce(std::result::Result<Option<Backup>, String>) + Send + 'static,
    ) -> Result<()> {
        ensure!(index < 2, "Unknown backup source");
        let (project, activity) = self.reserve();
        let engine = if index == 0 {
            DatabaseEngine::ClickHouse
        } else {
            DatabaseEngine::MySql
        };
        let prefix = project
            .databases
            .values()
            .find(|db| db.engine == engine)
            .context("Database source is not configured")?
            .prefix
            .clone();
        self.operations.spawn(Self::scope(&project),"Check S3 backups",move |job|async move {
            let _activity=activity;let mut cancel=job.cancellation();
            let request=async {let client=project.storage.client().await?;backups::latest_backup(&client,&project.storage.bucket,&prefix).await};
            let result=if job.flag.load(Ordering::Acquire){Err(anyhow::anyhow!("Cancelled"))}else{tokio::select!{result=request=>result,_=cancel.changed()=>Err(anyhow::anyhow!("Cancelled"))}}.map_err(|e|format!("{e:#}"));
            let outcome=result.as_ref().map(|_|()).map_err(|e|anyhow::anyhow!("{e}"));finished(result);outcome
        })?;
        Ok(())
    }
    pub fn download(
        self: &Arc<Self>,
        remote: [Backup; 2],
        progress: impl FnMut(Progress) + Send + 'static,
        finished: impl FnOnce(std::result::Result<Snapshot, DownloadError>) + Send + 'static,
    ) -> Result<UpdateControl> {
        let mut update = self.update.lock().unwrap();
        ensure!(update.is_none(), "A backup update is already running");
        let control = UpdateControl::new();
        *update = Some(control.clone());
        drop(update);
        let release = Updating(self.clone());
        let (project, activity) = self.reserve();
        let cache = self.cache.clone();
        let action = control.clone();
        let registration=self.operations.spawn(Self::scope(&project),"Download backups",move |job|async move {
            let _release=release;let _activity=activity;
            let mut cancel=job.cancellation();if job.flag.load(Ordering::Acquire){action.cancel();}
            let work=backup_cache::update(cache,project.storage,remote,action.clone(),progress);tokio::pin!(work);
            let result=tokio::select!{biased;_=cancel.changed()=>{action.cancel();work.await},result=&mut work=>result};
            complete(&job,&result);let outcome=outcome(&result);finished(result);outcome
        });
        registration?;
        Ok(control)
    }
    pub fn verify(
        self: &Arc<Self>,
        progress: impl FnMut(Progress) + Send + 'static,
        finished: impl FnOnce(std::result::Result<Snapshot, DownloadError>) + Send + 'static,
    ) -> Result<UpdateControl> {
        let mut update = self.update.lock().unwrap();
        ensure!(update.is_none(), "A backup operation is already running");
        let control = UpdateControl::new();
        *update = Some(control.clone());
        drop(update);
        let release = Updating(self.clone());
        let (project, activity) = self.reserve();
        let cache = self.cache.clone();
        let action = control.clone();
        self.operations.spawn(Self::scope(&project),"Verify backups",move |job|async move {
            let _release=release;let _activity=activity;let mut cancel=job.cancellation();
            if job.flag.load(Ordering::Acquire){action.cancel();}
            let check=action.clone();let mut worker=tokio::task::spawn_blocking(move ||backup_cache::verify_local(cache,check,progress));
            let joined=tokio::select!{biased;_=cancel.changed()=>{action.cancel();worker.await},result=&mut worker=>result};
            let result=joined.unwrap_or_else(|error|Err(DownloadError::Failed(error.to_string())));
            complete(&job,&result);let outcome=outcome(&result);finished(result);outcome
        })?;
        Ok(control)
    }
}
fn complete(job: &Operation, result: &std::result::Result<Snapshot, DownloadError>) {
    if matches!(result, Err(DownloadError::Cancelled)) {
        job.cancel();
    }
}
fn outcome(result: &std::result::Result<Snapshot, DownloadError>) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(DownloadError::Cancelled) => Err(anyhow::anyhow!("Cancelled")),
        Err(DownloadError::Failed(error)) => Err(anyhow::anyhow!("{error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelling_a_waiting_preparation_does_not_cancel_another_cache_operation() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-waiting-backups-{}",
            sandbox_platform::docker::nonce()?
        ));
        let executor = tokio::runtime::Runtime::new()?;
        let operations = Operations::new(executor.handle().clone());
        let catalog = Catalog::open(root.join("application.sqlite3"))?;
        let project = catalog.projects()?.remove(0);
        let service = Service::new(catalog, project.id, operations.clone())?;
        let guard = service.preparation.lock().unwrap();
        let foreign = UpdateControl::new();
        *service.update.lock().unwrap() = Some(foreign.clone());
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (send, receive) = std::sync::mpsc::channel();
        let result = std::thread::scope(|scope| {
            let worker = service.clone();
            let signal = cancel.clone();
            let job = scope.spawn(move || {
                worker.ensure_latest(&signal, move |_, _| {
                    let _ = send.send(());
                })
            });
            receive.recv().unwrap();
            cancel.store(true, Ordering::Release);
            job.join().unwrap()
        });
        assert!(result.is_err());
        assert!(
            foreign.cancel(),
            "Waiting subscriber must not cancel the owner of a shared cache update"
        );
        drop(guard);
        executor.block_on(operations.join());
        drop(service);
        fs_cleanup(root)?;
        Ok(())
    }
    fn fs_cleanup(root: std::path::PathBuf) -> Result<()> {
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
