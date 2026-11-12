//! Installation-scoped services. Windows only hold handles to this owner.
use anyhow::{Context, Result, ensure};
use sandbox_backups::service::Service as Backups;
use sandbox_core::{
    models::{ComposeLaunchSpec, Project, ProjectId, Sandbox, SandboxId, Scope, SourceMode},
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
    process_memory: Arc<sandbox_telemetry::process_log::Memory>,
    log_policy: Mutex<(usize, sandbox_core::LogSettings)>,
    stopping: AtomicBool,
    shutdown: Mutex<Shutdown>,
    exit_worker: Mutex<Option<thread::JoinHandle<()>>>,
    watchdog: Mutex<Option<Watchdog>>,
    _lock: File,
    shutdown_gate: Mutex<()>,
    source_gate: Mutex<()>,
}
pub mod launch;
impl Runtime {
    pub fn open(handle: tokio::runtime::Handle, settings: &Settings) -> Result<Arc<Self>> {
        // Native previews use disposable data and no helper sharing the app bundle identity.
        let preview = cfg!(debug_assertions)
            && std::env::var_os("SANDBOX_PREPARE_TEST_DATA_DIR").is_some()
            && std::env::var_os("SANDBOX_PREPARE_UI_PREVIEW").is_some();
        Self::at(database_path()?, handle, settings, !preview)
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
            process_memory: Arc::new(Default::default()),
            log_policy: Mutex::new((settings.current_log_limit, settings.logging)),
            stopping: AtomicBool::new(false),
            shutdown: Mutex::new(Shutdown::default()),
            exit_worker: Mutex::new(None),
            watchdog: Mutex::new(guard),
            _lock: lock,
            shutdown_gate: Mutex::new(()),
            source_gate: Mutex::new(()),
        });
        // Opening a backup service only registers secrets; it does not reread archives.
        for project in runtime.catalog.projects()? {
            runtime.backups(&project.id)?;
        }
        for sandbox in runtime.catalog.sandboxes()? {
            runtime.manager(&sandbox.id)?;
        }
        runtime.configure(settings);
        if watchdog {
            runtime.watch_docker()?;
        }
        Ok(runtime)
    }
    /// Runs inside a registered operation; no source files are rewritten.
    pub fn add_project(
        &self,
        name: String,
        location: String,
        clone_remote: bool,
        storage: sandbox_core::StorageConfig,
        cancel: &AtomicBool,
    ) -> Result<Project> {
        ensure!(!self.is_stopping(), "Application is shutting down");
        ensure!(!name.trim().is_empty(), "Enter a project name");
        let git = sandbox_projects::git::Git::default();
        let repository = if location.trim().is_empty() {
            None
        } else if clone_remote {
            let target = self
                .catalog
                .root
                .join("repositories")
                .join(docker::nonce()?);
            match git.clone_repository(location.trim(), &target, cancel, |_| {}) {
                Ok(repository) => Some(repository),
                Err(error) => {
                    if target.is_dir() {
                        let _ = fs::remove_dir_all(&target);
                    }
                    return Err(error);
                }
            }
        } else {
            Some(git.repository(std::path::Path::new(location.trim()), cancel)?)
        };
        let project = self.catalog.create_project(name, repository, storage)?;
        self.backups(&project.id)?;
        Ok(project)
    }
    pub fn create_sandbox(
        &self,
        project: &ProjectId,
        name: String,
        mode: Option<SourceMode>,
        cancel: &AtomicBool,
    ) -> Result<Sandbox> {
        self.create_source(project, name, mode, None, cancel)
    }
    /// Open the environment attached to a directory, or prepare its first environment.
    pub fn open_working_copy(
        &self,
        project: &ProjectId,
        mode: SourceMode,
        base_revision: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<Sandbox> {
        let project_record = self.catalog.project(project)?;
        let name = match &mode {
            SourceMode::ManagedWorktree { branch } => branch.trim().to_owned(),
            _ => {
                let repo = project_record
                    .repository
                    .as_ref()
                    .context("Open a Git project first")?;
                let trees =
                    sandbox_projects::git::Git::default().worktrees(&repo.directory, cancel)?;
                let tree = match &mode {
                    SourceMode::ExistingWorktree { directory } => {
                        let path = directory.canonicalize()?;
                        trees
                            .iter()
                            .find(|t| t.directory.canonicalize().ok().as_ref() == Some(&path))
                    }
                    _ => trees.first(),
                }
                .context("Working copy is unavailable")?;
                tree.branch.clone().unwrap_or_else(|| {
                    tree.directory
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
            }
        };
        self.create_source(project, name, Some(mode), base_revision, cancel)
    }
    fn create_source(
        &self,
        project: &ProjectId,
        name: String,
        mode: Option<SourceMode>,
        base_revision: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<Sandbox> {
        let _gate = self
            .source_gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Source preparation lock was poisoned"))?;
        ensure!(
            !self.is_stopping() && !cancel.load(Ordering::Acquire),
            "Source preparation cancelled"
        );
        let project = self.catalog.project(project)?;
        let git = sandbox_projects::git::Git::default();
        let directory = match &mode {
            Some(SourceMode::CurrentCheckout) => {
                let repo = project
                    .repository
                    .as_ref()
                    .context("Open a Git project first")?;
                Some(
                    git.worktrees(&repo.directory, cancel)?
                        .first()
                        .context("Main checkout missing")?
                        .directory
                        .canonicalize()?,
                )
            }
            Some(SourceMode::ExistingWorktree { directory }) => {
                let path = directory.canonicalize()?;
                let repo = project
                    .repository
                    .as_ref()
                    .context("Open a Git project first")?;
                ensure!(
                    git.worktrees(&repo.directory, cancel)?
                        .iter()
                        .any(|t| !t.prunable
                            && t.directory.canonicalize().ok().as_ref() == Some(&path)),
                    "Worktree does not belong to this project"
                );
                Some(path)
            }
            _ => None,
        };
        if let Some(directory) = directory {
            let matching: Vec<_> = self
                .catalog
                .sandboxes()?
                .into_iter()
                .filter(|s| {
                    s.source.as_ref().is_some_and(|source| {
                        source.directory == directory
                            || source.directory.canonicalize().ok().as_ref() == Some(&directory)
                    })
                })
                .collect();
            ensure!(
                matching.len() <= 1,
                "Multiple legacy environments use this working copy. Remove the redundant environments explicitly before opening it."
            );
            if let Some(existing) = matching.into_iter().next() {
                ensure!(
                    existing.project_id == project.id,
                    "This working copy belongs to another project"
                );
                self.manager(&existing.id)?;
                return Ok(existing);
            }
        }
        let mut sandbox = self.catalog.create_sandbox(project.id.clone(), name)?;
        if let Some(mode) = mode {
            if let Err(error) = git.prepare_from(
                &self.catalog,
                &project,
                &mut sandbox,
                mode,
                base_revision,
                cancel,
            ) {
                // Keep owned worktrees for reconciliation; an empty failed draft owns no resources.
                if sandbox.source.is_none() {
                    self.catalog.remove_sandbox(&sandbox.id)?;
                }
                return Err(error);
            }
        }
        if let Some(source) = &sandbox.source {
            if let Some(profile) = self.catalog.launch_profile(&sandbox.project_id)? {
                sandbox.compose = Some(sandbox_projects::launch_profile::resolve_paths(
                    profile,
                    &source.directory,
                ));
                self.catalog.save_sandbox(&sandbox)?;
            }
        }
        self.manager(&sandbox.id)?;
        Ok(sandbox)
    }
    pub fn remove_sandbox(&self, id: &SandboxId, cancel: &AtomicBool) -> Result<()> {
        ensure!(!self.is_stopping(), "Application is shutting down");
        let manager = self.manager(id)?;
        ensure!(
            !manager.snapshot().busy,
            "Wait for the current sandbox operation"
        );
        manager.launch(sandbox_sandboxes::sandbox::Action::Remove);
        while manager.snapshot().busy {
            if cancel.load(Ordering::Acquire) {
                manager.cancel();
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
        if let Some(error) = manager.snapshot().error {
            anyhow::bail!(error);
        }
        self.controllers.lock().unwrap().remove(id);
        Ok(())
    }
    pub fn configure_sandbox(&self, id: &SandboxId, spec: ComposeLaunchSpec) -> Result<()> {
        ensure!(!self.is_stopping(), "Application is shutting down");
        let manager = self.manager(id)?;
        ensure!(
            !manager.snapshot().busy,
            "Wait for the sandbox operation to finish"
        );
        let mut sandbox = self.catalog.sandbox(id)?;
        sandbox.compose = Some(spec);
        manager.save_configuration(|| self.catalog.save_sandbox(&sandbox))
    }
    fn watch_docker(self: &Arc<Self>) -> Result<()> {
        let runtime = Arc::downgrade(self);
        let owner = self.catalog.owner.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        self.operations.spawn_blocking(
            Scope::default(),
            "Observe Docker events",
            cancel,
            move |_| {
                let docker = Docker::discover();
                while !signal.load(Ordering::Acquire) {
                    let result = docker.run(
                        &docker::strings(&[
                            "events",
                            "--filter",
                            &format!("label=ru.mentormarketplace.sandbox-prepare.owner={owner}"),
                            "--format",
                            "{{json .}}",
                        ]),
                        None,
                        &signal,
                        std::time::Duration::from_secs(60),
                        |_| {
                            if let Some(runtime) = runtime.upgrade() {
                                let controllers: Vec<_> = runtime
                                    .controllers
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .cloned()
                                    .collect();
                                for manager in controllers {
                                    manager.launch(sandbox_sandboxes::sandbox::Action::Refresh);
                                }
                            }
                        },
                    );
                    if result.is_err() {
                        for _ in 0..50 {
                            if signal.load(Ordering::Acquire) {
                                break;
                            }
                            thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }
                Ok(())
            },
        )?;
        Ok(())
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
    pub fn sandbox_status(&self, id: &SandboxId) -> Option<sandbox_sandboxes::sandbox::State> {
        self.controllers
            .lock()
            .unwrap()
            .get(id)
            .map(|manager| manager.snapshot())
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
            self.process_memory.clone(),
        )?;
        let (limit, policy) = *self.log_policy.lock().unwrap();
        manager.logs.configure(limit, policy);
        controllers.insert(id.clone(), manager.clone());
        Ok(manager)
    }
    pub fn configure(&self, settings: &Settings) {
        self.limit.configure(settings.max_parallel_restores);
        *self.log_policy.lock().unwrap() = (settings.current_log_limit, settings.logging);
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
        sandbox_telemetry::logging::flush_if_initialized()?;
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
    fn working_copy_open_is_idempotent_concurrent_and_preserves_the_checkout() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("sandbox-source-runtime-{}", docker::nonce()?));
        let repo = root.join("repo");
        fs::create_dir_all(&repo)?;
        let git = |args: &[&str]| -> Result<String> {
            let result = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()?;
            ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            Ok(String::from_utf8(result.stdout)?)
        };
        git(&["init", "-b", "dev"])?;
        git(&["config", "user.name", "Fixture"])?;
        git(&["config", "user.email", "fixture@example.invalid"])?;
        git(&["config", "commit.gpgsign", "false"])?;
        git(&["config", "core.hooksPath", "/dev/null"])?;
        fs::write(repo.join("file"), "committed")?;
        git(&["add", "file"])?;
        git(&["commit", "-m", "fixture"])?;
        fs::write(repo.join("file"), "dirty local checkout")?;
        git(&["branch", "free"])?;
        let executor = tokio::runtime::Runtime::new()?;
        let app = Runtime::at(
            root.join("data/application.sqlite3"),
            executor.handle().clone(),
            &Settings::default(),
            false,
        )?;
        let repository =
            sandbox_projects::git::Git::default().repository(&repo, &AtomicBool::new(false))?;
        let project =
            app.catalog
                .create_project("Fixture".into(), Some(repository), Default::default())?;
        let ids = std::thread::scope(|scope| -> Result<Vec<SandboxId>> {
            let jobs: Vec<_> = (0..4)
                .map(|_| {
                    let app = app.clone();
                    let project = project.id.clone();
                    scope.spawn(move || {
                        app.open_working_copy(
                            &project,
                            SourceMode::CurrentCheckout,
                            None,
                            &AtomicBool::new(false),
                        )
                        .map(|s| s.id)
                    })
                })
                .collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect()
        })?;
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(app.catalog.sandboxes()?.len(), 1);
        #[cfg(unix)]
        {
            let alias = root.join("alias");
            std::os::unix::fs::symlink(&repo, &alias)?;
            let same = app.open_working_copy(
                &project.id,
                SourceMode::ExistingWorktree { directory: alias },
                None,
                &AtomicBool::new(false),
            )?;
            assert_eq!(same.id, ids[0]);
            assert_eq!(same.name, "dev");
        }
        let mut selected = app.catalog.sandbox(&ids[0])?;
        let profile = ComposeLaunchSpec {
            files: vec!["compose.yml".into()],
            automatic_connections: Some(sandbox_core::models::DatabaseNames {
                mysql: "app".into(),
                clickhouse: "analytics".into(),
            }),
            ..Default::default()
        };
        selected.compose = Some(profile.clone());
        app.catalog.save_launch_profile(&selected, &profile)?;
        let isolated = app.open_working_copy(
            &project.id,
            SourceMode::ManagedWorktree {
                branch: "free".into(),
            },
            None,
            &AtomicBool::new(false),
        )?;
        assert_ne!(isolated.id, ids[0]);
        assert_eq!(isolated.compose.as_ref().unwrap().files, profile.files);
        assert_eq!(
            isolated
                .compose
                .as_ref()
                .unwrap()
                .automatic_connections
                .as_ref()
                .unwrap()
                .clickhouse,
            "analytics"
        );
        assert!(isolated.source.as_ref().unwrap().owned);
        let reopened = app.open_working_copy(
            &project.id,
            SourceMode::ExistingWorktree {
                directory: isolated.source.unwrap().directory,
            },
            None,
            &AtomicBool::new(false),
        )?;
        assert_eq!(reopened.id, isolated.id);
        assert!(reopened.source.unwrap().owned);
        assert_eq!(git(&["branch", "--show-current"])?.trim(), "dev");
        assert_eq!(
            fs::read_to_string(repo.join("file"))?,
            "dirty local checkout"
        );
        let count = app.catalog.sandboxes()?.len();
        assert!(
            app.open_working_copy(
                &project.id,
                SourceMode::ManagedWorktree {
                    branch: "invalid..branch".into()
                },
                None,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert_eq!(app.catalog.sandboxes()?.len(), count);
        app.shutdown_blocking()?;
        drop(app);
        let reopened = Catalog::open(root.join("data/application.sqlite3"))?;
        assert_eq!(reopened.sandboxes()?.len(), count);
        drop(reopened);
        fs::remove_dir_all(root)?;
        Ok(())
    }
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

#[cfg(test)]
mod docker_tests {
    use super::*;
    use sandbox_sandboxes::{
        sandbox::MYSQL_IMAGE,
        sandbox_store::{Instance, Saved},
    };
    use std::time::{Duration, Instant};
    struct Cleanup {
        docker: Docker,
        names: Vec<String>,
        root: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for name in &self.names {
                let _ = self.docker.output(
                    &docker::strings(&["rm", "-f", "-v", name]),
                    Duration::from_secs(30),
                );
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    #[ignore = "Requires Docker; starts only disposable sleeping containers, no database data"]
    fn global_shutdown_stops_all_owned_services_and_keeps_an_unowned_generation() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("sandbox-runtime-docker-{}", docker::nonce()?));
        let executor = tokio::runtime::Runtime::new()?;
        let app = Runtime::at(
            root.join("application.sqlite3"),
            executor.handle().clone(),
            &Settings::default(),
            false,
        )?;
        let project = app.catalog.projects()?.remove(0);
        let docker = Docker::discover();
        let mut cleanup = Cleanup {
            docker: docker.clone(),
            names: vec![],
            root: root.clone(),
        };
        for index in 0..2 {
            let record = app
                .catalog
                .create_sandbox(project.id.clone(), format!("Fixture {index}"))?;
            let manager = app.manager(&record.id)?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while manager.snapshot().busy {
                ensure!(Instant::now() < deadline, "Preflight timed out");
                thread::sleep(Duration::from_millis(50));
            }
            let id = docker::nonce()?;
            let instance = Instance {
                id: id.clone(),
                project: format!("sp-{}-{}", &app.catalog.owner[..12], &id[..12]),
                ports: [0; 3],
                dynamic_ports: false,
                backups: Default::default(),
                backup_set: None,
                created: String::new(),
                tables: [0; 2],
            };
            manager.store.save(&Saved {
                active: Some(instance.clone()),
                ..Default::default()
            })?;
            for service in ["mysql", "clickhouse", "app-api"] {
                let name = manager.store.container(&instance, service);
                cleanup.names.push(name.clone());
                docker.output(
                    &docker::strings(&[
                        "run",
                        "-d",
                        "--name",
                        &name,
                        "--network",
                        "none",
                        "--label",
                        &format!(
                            "ru.mentormarketplace.sandbox-prepare.owner={}",
                            app.catalog.owner
                        ),
                        "--label",
                        &format!("ru.mentormarketplace.sandbox-prepare.instance={id}"),
                        "--entrypoint",
                        "sleep",
                        MYSQL_IMAGE,
                        "120",
                    ]),
                    Duration::from_secs(30),
                )?;
            }
        }
        let foreign = format!("sp-foreign-{}", &docker::nonce()?[..12]);
        cleanup.names.push(foreign.clone());
        docker.output(
            &docker::strings(&[
                "run",
                "-d",
                "--name",
                &foreign,
                "--network",
                "none",
                "--label",
                &format!(
                    "ru.mentormarketplace.sandbox-prepare.owner={}",
                    app.catalog.owner
                ),
                "--label",
                "ru.mentormarketplace.sandbox-prepare.instance=unowned",
                "--entrypoint",
                "sleep",
                MYSQL_IMAGE,
                "120",
            ]),
            Duration::from_secs(30),
        )?;
        app.shutdown_blocking()?;
        for name in &cleanup.names {
            let running = docker.output(
                &docker::strings(&["inspect", "--format", "{{.State.Running}}", name]),
                Duration::from_secs(10),
            )?;
            assert_eq!(
                running.trim(),
                if *name == foreign { "true" } else { "false" }
            );
        }
        drop(app);
        drop(cleanup);
        Ok(())
    }
}
