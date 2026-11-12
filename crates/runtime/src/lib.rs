//! Installation-scoped services. Windows only hold handles to this owner.
use anyhow::{Context, Result, ensure};
use sandbox_backups::service::Service as Backups;
use sandbox_core::{
    models::{ProjectId, SandboxId},
    operations::{Operations, RestoreLimit},
};
use sandbox_platform::{
    catalog::Catalog,
    database::database_path,
    docker::{self, Docker},
};
use sandbox_sandboxes::sandbox::{Manager, Watchdog};
use sandbox_settings::settings::Settings;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[derive(Clone, Default)]
pub struct Shutdown {
    pub running: bool,
    pub finished: bool,
    pub error: Option<String>,
}
pub struct Runtime {
    pub catalog: Catalog,
    pub operations: Arc<Operations>,
    pub limit: Arc<RestoreLimit>,
    controllers: Mutex<BTreeMap<SandboxId, Arc<Manager>>>,
    backups: Mutex<BTreeMap<ProjectId, Arc<Backups>>>,
    session: String,
    stopping: AtomicBool,
    shutdown: Mutex<Shutdown>,
    exit_worker: Mutex<Option<thread::JoinHandle<()>>>,
    watchdog: Mutex<Option<Watchdog>>,
    _lock: File,
    shutdown_gate: Mutex<()>,
}
impl Runtime {
    pub fn open(handle: tokio::runtime::Handle, settings: &Settings) -> Result<Arc<Self>> {
        Self::at(database_path()?, handle, settings, true)
    }
    pub fn at(
        database: PathBuf,
        handle: tokio::runtime::Handle,
        settings: &Settings,
        watchdog: bool,
    ) -> Result<Arc<Self>> {
        let root = database.parent().context("Invalid application directory")?;
        fs::create_dir_all(root.join("sandboxes"))?;
        // Same installation lock as the previous single-sandbox release.
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("sandboxes/.lock"))?;
        lock.try_lock()
            .context("Another Sandbox Prepare instance is managing these resources")?;
        let catalog = Catalog::open(database)?;
        let session = docker::nonce()?;
        let guard = if watchdog {
            Some(Watchdog::arm(
                &catalog.owner,
                &Docker::discover(),
                &session,
            )?)
        } else {
            None
        };
        let runtime = Arc::new(Self {
            catalog,
            operations: Operations::new(handle),
            limit: RestoreLimit::new(settings.max_parallel_restores),
            controllers: Mutex::new(BTreeMap::new()),
            backups: Mutex::new(BTreeMap::new()),
            session,
            stopping: AtomicBool::new(false),
            shutdown: Mutex::new(Shutdown::default()),
            exit_worker: Mutex::new(None),
            watchdog: Mutex::new(guard),
            _lock: lock,
            shutdown_gate: Mutex::new(()),
        });
        // Opening a backup service only registers secrets; it does not reread archives.
        for project in runtime.catalog.projects()? {
            runtime.backups(&project.id)?;
        }
        for sandbox in runtime.catalog.sandboxes()? {
            runtime.manager(&sandbox.id)?;
        }
        runtime.configure(settings);
        Ok(runtime)
    }
    pub fn backups(&self, id: &ProjectId) -> Result<Arc<Backups>> {
        let mut services = self.backups.lock().unwrap();
        if let Some(service) = services.get(id) {
            return Ok(service.clone());
        }
        ensure!(!self.is_stopping(), "Application is shutting down");
        let service = Backups::new(self.catalog.clone(), id.clone(), self.operations.clone())?;
        services.insert(id.clone(), service.clone());
        Ok(service)
    }
    pub fn manager(&self, id: &SandboxId) -> Result<Arc<Manager>> {
        let mut controllers = self.controllers.lock().unwrap();
        if let Some(manager) = controllers.get(id) {
            return Ok(manager.clone());
        }
        ensure!(!self.is_stopping(), "Application is shutting down");
        let manager = Manager::scoped(
            self.catalog.clone(),
            self.catalog.sandbox(id)?,
            self.operations.clone(),
            self.limit.clone(),
            self.session.clone(),
        )?;
        controllers.insert(id.clone(), manager.clone());
        Ok(manager)
    }
    pub fn configure(&self, settings: &Settings) {
        self.limit.configure(settings.max_parallel_restores);
        for manager in self.controllers.lock().unwrap().values() {
            manager
                .logs
                .configure(settings.current_log_limit, settings.logging);
        }
    }
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
    pub fn shutdown_status(&self) -> Shutdown {
        self.shutdown.lock().unwrap().clone()
    }
    pub fn exit_ready(&self) -> bool {
        self.shutdown.lock().unwrap().finished
    }
    pub fn begin_shutdown(self: &Arc<Self>) {
        let mut state = self.shutdown.lock().unwrap();
        if state.running || state.finished {
            return;
        }
        if let Some(previous) = self.exit_worker.lock().unwrap().take() {
            let _ = previous.join();
        }
        self.stopping.store(true, Ordering::Release);
        self.operations.stop();
        *state = Shutdown {
            running: true,
            ..Default::default()
        };
        let runtime = self.clone();
        let worker = thread::spawn(move || {
            let result = runtime.shutdown_blocking();
            *runtime.shutdown.lock().unwrap() = match result {
                Ok(()) => Shutdown {
                    finished: true,
                    ..Default::default()
                },
                Err(error) => Shutdown {
                    error: Some(format!(
                        "Could not stop all sandboxes: {error:#}. Close again to retry."
                    )),
                    ..Default::default()
                },
            };
        });
        *self.exit_worker.lock().unwrap() = Some(worker);
    }
    pub fn shutdown_blocking(&self) -> Result<()> {
        let _gate = self.shutdown_gate.lock().unwrap();
        self.stopping.store(true, Ordering::Release);
        self.operations.stop();
        self.operations.handle().block_on(self.operations.join());
        let controllers: Vec<_> = self.controllers.lock().unwrap().values().cloned().collect();
        let results = thread::scope(|scope| {
            let workers: Vec<_> = controllers
                .iter()
                .map(|manager| scope.spawn(move || manager.shutdown_blocking()))
                .collect();
            workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("Shutdown worker panicked")))
                })
                .collect::<Vec<_>>()
        });
        let errors: Vec<_> = results
            .into_iter()
            .filter_map(Result::err)
            .map(|e| format!("{e:#}"))
            .collect();
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        if let Some(watchdog) = self.watchdog.lock().unwrap().take() {
            watchdog.disarm();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_rejects_a_second_owner_and_shuts_down_without_docker_resources() -> Result<()> {
        let root = std::env::temp_dir().join(format!("sandbox-runtime-{}", docker::nonce()?));
        fs::create_dir_all(&root)?;
        let executor = tokio::runtime::Runtime::new()?;
        let app = Runtime::at(
            root.join("application.sqlite3"),
            executor.handle().clone(),
            &Settings::default(),
            false,
        )?;
        assert!(
            Runtime::at(
                root.join("application.sqlite3"),
                executor.handle().clone(),
                &Settings::default(),
                false
            )
            .is_err()
        );
        app.shutdown_blocking()?;
        assert!(
            app.operations
                .spawn(Default::default(), "late", |_| async { Ok(()) })
                .is_err()
        );
        drop(app);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
