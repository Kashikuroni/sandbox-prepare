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
