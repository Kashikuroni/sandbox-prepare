//! Owns only this installation's labelled Docker resources. Restores publish a pair.
use crate::{
    backup_cache::FILES,
    docker::{Docker, strings},
    process_log::ProcessLog,
    sandbox_store::{Instance, Store, service_file},
};
use anyhow::{Context, Result, ensure};
use sandbox_core::{
    models::{Sandbox, Scope},
    operations::{Operations, RestoreLimit},
};
use sandbox_platform::catalog::Catalog;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::TcpListener,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const MYSQL_IMAGE: &str = "percona/percona-server:8.4.11-11@sha256:95d276d7f2b2718b725ad834a4d8a6f9e855b9566e51be8a9ff98d54b675f644";
pub const CH_IMAGE: &str = "clickhouse/clickhouse-server:26.8.19.9@sha256:3ddc359dd4c29921f1b926559344c14c7d98f024e1db9d9d02926e182426fba2";
pub const XB_IMAGE: &str = "percona/percona-xtrabackup:8.4@sha256:3b8032f97ba8e5c184803d0b644a893e80a613a9ca7806eb386aa5fb313778ff";
const OWNER_LABEL: &str = "ru.mentormarketplace.sandbox-prepare.owner";
const SESSION_LABEL: &str = "ru.mentormarketplace.sandbox-prepare.session";
const INSTANCE_LABEL: &str = "ru.mentormarketplace.sandbox-prepare.instance";
const SHORT: Duration = Duration::from_secs(30);
const LONG: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Clone, Default)]
pub struct State {
    pub active: Option<Instance>,
    pub busy: bool,
    pub stage: String,
    pub error: Option<String>,
    pub running: [bool; 2],
    pub docker_ready: bool,
    pub shutting_down: bool,
}
#[derive(Clone, Copy)]
pub enum Action {
    Refresh,
    Create,
    Start,
    Stop,
    Delete,
}
pub struct Watchdog {
    input: Option<ChildStdin>,
    child: Child,
}
impl Watchdog {
    pub fn arm(owner: &str, docker: &Docker, session: &str) -> Result<Self> {
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                std::ffi::OsStr::new("--sandbox-watchdog"),
                std::ffi::OsStr::new(owner),
                docker.binary.as_os_str(),
                std::ffi::OsStr::new(session),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not start sandbox lifecycle watchdog")?;
        Ok(Self {
            input: child.stdin.take(),
            child,
        })
    }
    pub fn disarm(mut self) {
        if let Some(mut input) = self.input.take() {
            let _ = input.write_all(b"clean");
        }
        let _ = self.child.wait();
    }
}
pub struct Manager {
    pub store: Store,
    pub logs: Arc<ProcessLog>,
    pub revision: AtomicU64,
    pub extracted_bytes: Arc<AtomicU64>,
    docker: Docker,
    state: Mutex<State>,
    cancel: Arc<AtomicBool>,
    session: String,
    reconciled: AtomicBool,
    shutdown: AtomicBool,
    followers: Mutex<Vec<(Arc<AtomicBool>, thread::JoinHandle<()>)>>,
    watchdog: Mutex<Option<Watchdog>>,
    _lock: File,
    operations: Option<Arc<Operations>>,
    limit: Arc<RestoreLimit>,
    scope: Scope,
}
impl Manager {
    pub fn scoped(
        catalog: Catalog,
        sandbox: Sandbox,
        operations: Arc<Operations>,
        limit: Arc<RestoreLimit>,
        session: String,
    ) -> Result<Arc<Self>> {
        let scope = Scope {
            project: Some(sandbox.project_id.clone()),
            sandbox: Some(sandbox.id.clone()),
            operation: None,
        };
        let locks = catalog.root.join("sandboxes/.locks");
        fs::create_dir_all(&locks)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(locks.join(format!("{}.lock", sandbox.id)))?;
        lock.try_lock()
            .context("Sandbox is already managed by another controller")?;
        let store = Store::scoped(catalog, sandbox)?;
        let active = store.load()?.active;
        let logs = Arc::new(ProcessLog::new(
            store.root.join("process-logs"),
            vec![store.password.clone(), store.root_password.clone()],
        ));
        let manager = Arc::new(Self {
            store,
            logs,
            revision: AtomicU64::new(1),
            extracted_bytes: Arc::new(AtomicU64::new(0)),
            docker: Docker::discover(),
            state: Mutex::new(State {
                active,
                ..Default::default()
            }),
            cancel: Arc::new(AtomicBool::new(false)),
            session,
            reconciled: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            followers: Mutex::new(vec![]),
            watchdog: Mutex::new(None),
            _lock: lock,
            operations: Some(operations),
            limit,
            scope,
        });
        manager.launch(Action::Refresh);
        Ok(manager)
    }
    pub fn open() -> Result<Arc<Self>> {
        Self::at(crate::database::database_path()?, true)
    }
    pub fn at(database: std::path::PathBuf, watchdog: bool) -> Result<Arc<Self>> {
        let store = Store::open(database)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(store.root.join(".lock"))?;
        lock.try_lock()
            .context("Another Sandbox Prepare instance owns these databases. Close it first.")?;
        let active = store.load()?.active;
        let logs = Arc::new(ProcessLog::new(
            store.root.join("process-logs"),
            vec![store.password.clone(), store.root_password.clone()],
        ));
        let manager = Arc::new(Self {
            store,
            logs,
            revision: AtomicU64::new(1),
            extracted_bytes: Arc::new(AtomicU64::new(0)),
            docker: Docker::discover(),
            state: Mutex::new(State {
                active,
                ..Default::default()
            }),
            cancel: Arc::new(AtomicBool::new(false)),
            session: crate::docker::nonce()?,
            reconciled: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            followers: Mutex::new(vec![]),
            watchdog: Mutex::new(None),
            _lock: lock,
            operations: None,
            limit: RestoreLimit::new(2),
            scope: Scope::default(),
        });
        if watchdog {
            manager.arm_watchdog()?;
        }
        manager.launch(Action::Refresh);
        Ok(manager)
    }
    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }
    fn change(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        self.revision.fetch_add(1, Ordering::Release);
    }
    fn stage(&self, text: &str) {
        self.change(|s| s.stage = text.into());
        tracing::info!(target:"app::sandbox",stage=text,"Sandbox operation");
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        self.stage("Cancelling…");
    }
    pub fn launch(self: &Arc<Self>, action: Action) {
        {
            let mut state = self.state.lock().unwrap();
            if state.busy || self.shutdown.load(Ordering::Acquire) {
                return;
            }
            state.busy = true;
            state.error = None;
        }
        self.cancel.store(false, Ordering::Release);
        self.revision.fetch_add(1, Ordering::Release);
        let manager = self.clone();
        let work = move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match action {
                Action::Refresh => manager.refresh(),
                Action::Create => manager.restore(),
                Action::Start => manager.start(),
                Action::Stop => manager.stop(),
                Action::Delete => manager.delete(),
            }))
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "Sandbox worker failed unexpectedly. Refresh to reconcile its resources."
                ))
            });
            if let Err(error) = &result {
                manager.logs.append("Lifecycle", format!("{error:#}"));
                tracing::error!(target:"app::sandbox",%error,"Sandbox operation failed");
            }
            let outcome = result
                .as_ref()
                .map(|_| ())
                .map_err(|error| anyhow::anyhow!("{error:#}"));
            manager.change(|s| {
                s.busy = false;
                s.error = result.err().map(|e| format!("{e:#}"));
                if s.error.is_none() {
                    s.stage = "Ready".into();
                }
            });
            outcome
        };
        if let Some(operations) = &self.operations {
            if let Err(error) = operations.spawn_blocking(
                self.scope.clone(),
                "Sandbox operation",
                self.cancel.clone(),
                move |job| {
                    job.running();
                    work()
                },
            ) {
                self.change(|state| {
                    state.busy = false;
                    state.error = Some(error.to_string());
                });
            }
        } else {
            thread::spawn(work);
        }
    }
    fn run(
        &self,
        args: &[String],
        input: Option<Box<dyn Read + Send>>,
        process: &str,
        timeout: Duration,
    ) -> Result<String> {
        self.docker
            .run(args, input, &self.cancel, timeout, |line| {
                self.logs.append(process, line)
            })
            .map_err(|error| {
                let message = format!("{error:#}")
                    .replace(&self.store.password, "[redacted]")
                    .replace(&self.store.root_password, "[redacted]");
                anyhow::anyhow!(message)
            })
    }
    fn compose(&self, instance: &Instance, args: &[&str]) -> Result<String> {
        self.store.validate(instance)?;
        let mut command = strings(&["compose", "--project-name", &instance.project, "--file"]);
        command.push(self.store.compose(instance)?.display().to_string());
        command.extend(strings(args));
        self.run(&command, None, "Compose", LONG)
    }
    fn owned_containers(&self) -> Result<Vec<String>> {
        if self.store.scope.is_some() {
            let saved = self.store.load()?;
            let mut ids = Vec::new();
            for instance in saved
                .active
                .iter()
                .chain(saved.pending.iter())
                .chain(saved.retired.iter())
            {
                self.store.validate(instance)?;
                let output = self.docker.output(
                    &strings(&[
                        "ps",
                        "-aq",
                        "--filter",
                        &format!("label={OWNER_LABEL}={}", self.store.owner),
                        "--filter",
                        &format!("label={INSTANCE_LABEL}={}", instance.id),
                    ]),
                    SHORT,
                )?;
                ids.extend(
                    output
                        .lines()
                        .filter(|line| !line.is_empty())
                        .map(str::to_owned),
                );
            }
            ids.sort();
            ids.dedup();
            return Ok(ids);
        }
        let output = self.docker.output(
            &strings(&[
                "ps",
                "-aq",
                "--filter",
                &format!("label={OWNER_LABEL}={}", self.store.owner),
            ]),
            SHORT,
        )?;
        Ok(output
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }
    fn stop_all(&self) -> Result<()> {
        self.stop_followers();
        let ids = self.owned_containers()?;
        if !ids.is_empty() {
            let mut args = strings(&["stop", "--timeout", "15"]);
            args.extend(ids);
            self.docker.output(&args, Duration::from_secs(45))?;
        }
        if self.store.scope.is_some() {
            for id in self.owned_containers()? {
                let status = self.docker.output(
                    &strings(&["inspect", "--format", "{{.State.Running}}", &id]),
                    SHORT,
                )?;
                ensure!(
                    status.trim() != "true",
                    "A sandbox container is still running"
                );
            }
            self.change(|s| s.running = [false; 2]);
            return Ok(());
        }
        let running = self.docker.output(
            &strings(&[
                "ps",
                "-q",
                "--filter",
                &format!("label={OWNER_LABEL}={}", self.store.owner),
            ]),
            SHORT,
        )?;
        ensure!(
            running.trim().is_empty(),
            "Some sandbox containers are still running"
        );
        self.change(|s| s.running = [false; 2]);
        Ok(())
    }
    fn refresh(&self) -> Result<()> {
        self.stage("Checking Docker…");
        self.docker.check()?;
        // Reconcile after a crash. Only our pending generation is disposable.
        let mut saved = self.store.load()?;
        if !self.reconciled.swap(true, Ordering::AcqRel) {
            self.stop_all()?;
        }
        if let Some(pending) = saved.pending.take() {
            self.remove_instance(&pending)?;
            self.store.save(&saved)?;
        }
        let mut remaining = vec![];
        for retired in saved.retired.drain(..) {
            if self.remove_instance(&retired).is_err() {
                remaining.push(retired);
            }
        }
        saved.retired = remaining;
        self.store.save(&saved)?;
        self.change(|s| {
            s.docker_ready = true;
            s.active = saved.active.clone();
        });
        self.inspect_active()?;
        Ok(())
    }
    fn inspect_active(&self) -> Result<()> {
        if let Some(instance) = self.snapshot().active {
            let mut running = [false; 2];
            for (index, service) in ["mysql", "clickhouse"].into_iter().enumerate() {
                let output = self.docker.output(
                    &strings(&[
                        "inspect",
                        "--format",
                        "{{.State.Running}}",
                        &self.store.container(&instance, service),
                    ]),
                    SHORT,
                )?;
                running[index] = output.trim() == "true";
            }
            self.change(|s| s.running = running);
            self.follow(&instance);
        }
        Ok(())
    }
    fn pull(&self) -> Result<()> {
        for (label, image) in [
            ("Percona 8.4.11-11", MYSQL_IMAGE),
            ("ClickHouse 26.8.19.9", CH_IMAGE),
            ("XtraBackup 8.4", XB_IMAGE),
        ] {
            self.stage(&format!("Preparing image: {label}…"));
            if self
                .docker
                .output(&strings(&["image", "inspect", image]), SHORT)
                .is_err()
            {
                self.run(&strings(&["pull", image]), None, "Images", LONG)?;
            }
        }
        Ok(())
    }
    fn restore(&self) -> Result<()> {
        self.stage("Checking local backups…");
        let lease = self.store.cache()?.lease()?;
        self.stage("Waiting for a restore slot…");
        let _permit = self.limit.acquire(&self.cancel)?;
        self.docker.check()?;
        self.pull()?;
        let mut saved = self.store.load()?;
        ensure!(
            saved.pending.is_none(),
            "An interrupted restore needs reconciliation. Refresh first."
        );
        let id = crate::docker::nonce()?;
        let project = format!("sp-{}-{}", &self.store.owner[..12], &id[..12]);
        let mut instance = Instance {
            id,
            project,
            ports: [free_port(13306)?, free_port(18123)?, free_port(19000)?],
            backups: std::array::from_fn(|i| lease.pair.records[i].key.clone()),
            created: chrono::Utc::now().to_rfc3339(),
            tables: [0; 2],
        };
        self.write_config(&instance, Some(&lease.directory.join(FILES[0])))?;
        saved.pending = Some(instance.clone());
        self.store.save(&saved)?;
        let restored = (|| -> Result<()> {
            self.stage("Creating isolated database volumes…");
            self.compose(
                &instance,
                &["create", "--pull", "never", "mysql", "clickhouse"],
            )?;
            self.ensure_space(&instance, lease.pair.records.iter().map(|r| r.size).sum())?;
            let uid = self.run(&self.probe_args(&instance, "-u"), None, "Preflight", SHORT)?;
            let gid = self.run(&self.probe_args(&instance, "-g"), None, "Preflight", SHORT)?;
            let uid = uid
                .trim()
                .parse::<u32>()
                .context("Could not determine MySQL UID")?;
            let gid = gid
                .trim()
                .parse::<u32>()
                .context("Could not determine MySQL GID")?;
            self.stage("Extracting MySQL backup…");
            let input = File::open(lease.directory.join(FILES[1]))?;
            let reader = zstd::stream::read::Decoder::new(input)?;
            let cancel = &self.cancel;
            self.extracted_bytes.store(0, Ordering::Release);
            let stage = self.extracted_bytes.clone();
            let read = CountedReader {
                inner: reader,
                cancel: self.cancel.clone(),
                bytes: stage.clone(),
                reported: Instant::now(),
                log: self.logs.clone(),
            };
            let args = self.helper_args(
                &instance,
                "extract",
                "test -z \"$(ls -A /var/lib/mysql)\"; exec xbstream -x -C /var/lib/mysql",
            );
            self.run(&args, Some(Box::new(read)), "MySQL restore", LONG)?;
            ensure!(!cancel.load(Ordering::Acquire), "Restore cancelled");
            self.stage("Preparing MySQL backup (apply redo)…");
            let script = format!(
                "test -s /var/lib/mysql/ibdata1; test -s /var/lib/mysql/xtrabackup_info; test -s /var/lib/mysql/xtrabackup_checkpoints; grep -Eq '^server_version[[:space:]]*=[[:space:]]*8\\.4\\.11(-11)?([[:space:]]|$)' /var/lib/mysql/xtrabackup_info; grep -Eq '^partial[[:space:]]*=[[:space:]]*N' /var/lib/mysql/xtrabackup_info; grep -Eq '^incremental[[:space:]]*=[[:space:]]*N' /var/lib/mysql/xtrabackup_info; xtrabackup --prepare --use-memory=1G --target-dir=/var/lib/mysql; grep -Eq '^backup_type[[:space:]]*=[[:space:]]*full-prepared' /var/lib/mysql/xtrabackup_checkpoints; chown -R {uid}:{gid} /var/lib/mysql"
            );
            self.run(
                &self.helper_args(&instance, "prepare", &script),
                None,
                "MySQL prepare",
                LONG,
            )?;
            self.stage("Starting restored databases…");
            self.compose(
                &instance,
                &[
                    "up",
                    "-d",
                    "--no-build",
                    "--pull",
                    "never",
                    "mysql",
                    "clickhouse",
                ],
            )?;
            self.follow(&instance);
            self.wait_ready(&instance)?;
            self.stage("Restoring ClickHouse databases…");
            self.sql(&instance,false,"RESTORE ALL EXCEPT DATABASE system, INFORMATION_SCHEMA, information_schema FROM Disk('backups', 'clickhouse.tar.zst')")?;
            self.stage("Checking restored databases and table reads…");
            instance.tables = self.validate_tables(&instance)?;
            self.stage("Detaching the backup archive…");
            self.write_config(&instance, None)?;
            self.compose(
                &instance,
                &[
                    "up",
                    "-d",
                    "--no-build",
                    "--pull",
                    "never",
                    "--force-recreate",
                    "clickhouse",
                ],
            )?;
            self.follow(&instance);
            self.wait_ready(&instance)?;
            ensure!(
                !self.cancel.load(Ordering::Acquire),
                "Restore cancelled before activation"
            );
            Ok(())
        })();
        if let Err(error) = restored {
            // The previous active pair is never touched by extraction or prepare.
            if self.remove_instance(&instance).is_ok() {
                saved.pending = None;
                self.store.save(&saved)?;
            }
            if let Some(old) = &saved.active {
                self.follow(old);
            }
            return Err(error);
        }
        // SQLite publishes both database identities together. Retain old resources if
        // the write fails or its result is ambiguous.
        if let Some(old) = saved.active.replace(instance.clone()) {
            saved.retired.push(old);
        }
        saved.pending = None;
        self.store.save(&saved)?;
        self.change(|s| {
            s.active = Some(instance.clone());
            s.running = [true; 2];
        });
        self.stage("Removing the previous sandbox generation…");
        saved
            .retired
            .retain(|old| self.remove_instance(old).is_err());
        self.store.save(&saved)?;
        Ok(())
    }
    fn probe_args(&self, instance: &Instance, flag: &str) -> Vec<String> {
        strings(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--label",
            &format!("{OWNER_LABEL}={}", self.store.owner),
            "--label",
            &format!("{INSTANCE_LABEL}={}", instance.id),
            "--label",
            &format!("{SESSION_LABEL}={}", self.session),
            "--entrypoint",
            "id",
            MYSQL_IMAGE,
            flag,
            "mysql",
        ])
    }
    fn helper_args(&self, instance: &Instance, suffix: &str, script: &str) -> Vec<String> {
        strings(&[
            "run",
            "--rm",
            "-i",
            "--name",
            &self.store.container(instance, suffix),
            "--label",
            &format!("{OWNER_LABEL}={}", self.store.owner),
            "--label",
            &format!("{INSTANCE_LABEL}={}", instance.id),
            "--label",
            &format!("{SESSION_LABEL}={}", self.session),
            "--network",
            "none",
            "--user",
            "0:0",
            "--memory",
            "2g",
            "--mount",
            &format!(
                "type=volume,source={},target=/var/lib/mysql",
                self.store.volume(instance, 0)
            ),
            "--entrypoint",
            "/bin/bash",
            XB_IMAGE,
            "-euo",
            "pipefail",
            "-c",
            script,
        ])
    }
    fn ensure_space(&self, instance: &Instance, compressed: u64) -> Result<()> {
        self.stage("Checking free space in Docker storage…");
        let output = self.run(
            &self.helper_args(instance, "space", "df -Pk /var/lib/mysql | tail -1"),
            None,
            "Preflight",
            SHORT,
        )?;
        let available = output
            .split_whitespace()
            .nth(3)
            .context("Could not read Docker free space")?
            .parse::<u64>()?
            * 1024;
        self.logs.append(
            "Preflight",
            format!(
                "Docker free space: {}. Expanded backup size is not known before extraction.",
                crate::backups::format_size(available)
            ),
        );
        ensure!(
            available > compressed.saturating_add(2 * 1024 * 1024 * 1024),
            "Insufficient Docker disk space even for compressed backups and reserve. Expanded data needs additional space."
        );
        Ok(())
    }
    fn sql(&self, instance: &Instance, mysql: bool, sql: &str) -> Result<String> {
        let service = if mysql { "mysql" } else { "clickhouse" };
        let mut args = strings(&["exec", "-i", &self.store.container(instance, service)]);
        if mysql {
            args.extend(strings(&[
                "mysql",
                "--defaults-extra-file=/tmp/sandbox-client.cnf",
                "--batch",
                "--skip-column-names",
            ]));
        } else {
            args.extend(strings(&[
                "clickhouse-client",
                "--config-file=/tmp/sandbox-client.xml",
                "--multiquery",
            ]));
        }
        self.run(
            &args,
            Some(Box::new(std::io::Cursor::new(sql.as_bytes().to_vec()))),
            if mysql {
                "MySQL checks"
            } else {
                "ClickHouse restore"
            },
            if sql.starts_with("RESTORE ") {
                LONG
            } else {
                SHORT
            },
        )
    }
    fn wait_ready(&self, instance: &Instance) -> Result<()> {
        let started = Instant::now();
        loop {
            ensure!(!self.cancel.load(Ordering::Acquire), "Operation cancelled");
            for service in ["mysql", "clickhouse"] {
                let status = self.docker.output(
                    &strings(&[
                        "inspect",
                        "--format",
                        "{{.State.Status}}",
                        &self.store.container(instance, service),
                    ]),
                    SHORT,
                )?;
                if matches!(status.trim(), "exited" | "dead") {
                    let _ = self.run(
                        &strings(&[
                            "logs",
                            "--tail",
                            "40",
                            &self.store.container(instance, service),
                        ]),
                        None,
                        service,
                        SHORT,
                    );
                    anyhow::bail!(
                        "{service} exited during startup. See Processes for the container log."
                    );
                }
            }
            let mysql = self.sql(instance, true, "SELECT 1;");
            let ch = self.sql(instance, false, "SELECT 1;");
            if mysql.is_ok() && ch.is_ok() {
                return Ok(());
            }
            ensure!(
                started.elapsed() < Duration::from_secs(180),
                "Databases did not become ready. See Processes for startup errors."
            );
            thread::sleep(Duration::from_secs(2));
        }
    }
    fn validate_tables(&self, instance: &Instance) -> Result<[u64; 2]> {
        let mysql=self.sql(instance,true,"SELECT COUNT(*) FROM information_schema.tables WHERE table_schema NOT IN ('mysql','sys','performance_schema','information_schema');")?.trim().parse()?;
        let ch=self.sql(instance,false,"SELECT count() FROM system.tables WHERE database NOT IN ('system','INFORMATION_SCHEMA','information_schema')")?.trim().parse()?;
        let mysql_reads=self.sql(instance,true,"SELECT CONCAT('SELECT 1 FROM `', REPLACE(table_schema,'`','``'), '`.`',REPLACE(table_name,'`','``'),'` LIMIT 1;') FROM information_schema.tables WHERE table_type='BASE TABLE' AND table_schema NOT IN ('mysql','sys','performance_schema','information_schema') ORDER BY table_schema,table_name LIMIT 20;")?;
        if !mysql_reads.trim().is_empty() {
            self.sql(instance, true, &mysql_reads)?;
        }
        // Sample stored ClickHouse tables without triggering external dictionaries/views.
        let ch_reads=self.sql(instance,false,"SELECT concat('SELECT 1 FROM `',replaceAll(database,'`','``'),'`.`',replaceAll(name,'`','``'),'` LIMIT 1;') FROM system.tables WHERE database NOT IN ('system','INFORMATION_SCHEMA','information_schema') AND (engine LIKE '%MergeTree' OR engine IN ('Log','TinyLog','StripeLog','Memory')) ORDER BY database,name LIMIT 20 FORMAT TSVRaw")?;
        if !ch_reads.trim().is_empty() {
            self.sql(instance, false, &ch_reads)?;
        }
        Ok([mysql, ch])
    }
    fn start(&self) -> Result<()> {
        let instance = self.snapshot().active.context("Create a sandbox first")?;
        self.stage("Starting databases…");
        self.write_config(&instance, None)?;
        self.compose(&instance, &["up", "-d", "--no-build", "--pull", "never"])?;
        self.follow(&instance);
        if let Err(error) = self.wait_ready(&instance) {
            let _ = self.stop_all();
            return Err(error);
        }
        self.change(|s| s.running = [true; 2]);
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        self.stage("Stopping databases…");
        self.stop_all()
    }
    fn delete(&self) -> Result<()> {
        self.stage("Removing sandbox databases…");
        self.stop_followers();
        let mut saved = self.store.load()?;
        if let Some(active) = saved.active.clone() {
            self.remove_instance(&active)?;
            saved.active = None;
        }
        self.store.save(&saved)?;
        self.change(|s| {
            s.active = None;
            s.running = [false; 2];
        });
        Ok(())
    }
    fn remove_instance(&self, instance: &Instance) -> Result<()> {
        self.store.validate(instance)?;
        let ids = self.docker.output(
            &strings(&[
                "ps",
                "-aq",
                "--filter",
                &format!("label={OWNER_LABEL}={}", self.store.owner),
                "--filter",
                &format!("label={INSTANCE_LABEL}={}", instance.id),
            ]),
            SHORT,
        )?;
        let ids: Vec<_> = ids.lines().filter(|l| !l.is_empty()).collect();
        if !ids.is_empty() {
            let mut args = strings(&["rm", "-f", "-v"]);
            args.extend(ids.iter().map(|id| (*id).into()));
            self.docker.output(&args, SHORT)?;
        }
        let volumes = self
            .docker
            .output(&strings(&["volume", "ls", "--format", "{{.Name}}"]), SHORT)?;
        for index in 0..2 {
            let volume = self.store.volume(instance, index);
            if volumes.lines().any(|name| name == volume) {
                let output = self
                    .docker
                    .output(&strings(&["volume", "inspect", &volume]), SHORT)?;
                let value: Value = serde_json::from_str(&output)?;
                ensure!(
                    value[0]["Labels"][OWNER_LABEL].as_str() == Some(&self.store.owner),
                    "Refusing to remove a foreign Docker volume"
                );
                self.docker
                    .output(&strings(&["volume", "rm", &volume]), SHORT)?;
            }
        }
        let network = format!("{}_default", instance.project);
        let networks = self
            .docker
            .output(&strings(&["network", "ls", "--format", "{{.Name}}"]), SHORT)?;
        if networks.lines().any(|name| name == network) {
            let output = self
                .docker
                .output(&strings(&["network", "inspect", &network]), SHORT)?;
            let value: Value = serde_json::from_str(&output)?;
            ensure!(
                value[0]["Labels"][OWNER_LABEL].as_str() == Some(&self.store.owner),
                "Refusing to remove a foreign network"
            );
            self.docker
                .output(&strings(&["network", "rm", &network]), SHORT)?;
        }
        let directory = self.store.directory(instance)?;
        if directory.exists() {
            fs::remove_dir_all(directory)?;
        }
        Ok(())
    }
    fn stop_followers(&self) {
        let followers: Vec<_> = self.followers.lock().unwrap().drain(..).collect();
        for (cancel, _) in &followers {
            cancel.store(true, Ordering::Release);
        }
        for (_, task) in followers {
            let _ = task.join();
        }
    }
    fn follow(&self, instance: &Instance) {
        self.stop_followers();
        for service in ["mysql", "clickhouse"] {
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            let docker = self.docker.clone();
            let logs = self.logs.clone();
            let name = self.store.container(instance, service);
            let task = thread::spawn(move || {
                let _ = docker.run(
                    &strings(&["logs", "--follow", "--timestamps", "--tail", "200", &name]),
                    None,
                    &cancel,
                    LONG,
                    |line| logs.append(service, line),
                );
            });
            self.followers.lock().unwrap().push((signal, task));
        }
    }
    pub fn begin_shutdown(self: &Arc<Self>) {
        if self.snapshot().shutting_down {
            return;
        }
        self.shutdown.store(true, Ordering::Release);
        self.cancel.store(true, Ordering::Release);
        self.change(|s| {
            s.shutting_down = true;
            s.stage = "Stopping sandbox before exit…".into();
        });
        let manager = self.clone();
        thread::spawn(move || {
            let result = manager.shutdown_blocking();
            manager.change(|s| {
                s.shutting_down = false;
                s.error = result
                    .err()
                    .map(|e| format!("Could not stop sandbox: {e:#}. Retry closing to try again."));
            });
        });
    }
    pub fn exit_ready(&self) -> bool {
        let state = self.snapshot();
        self.shutdown.load(Ordering::Acquire)
            && !state.shutting_down
            && !state.busy
            && state.error.is_none()
    }
    pub fn shutdown_blocking(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::Release);
        self.cancel.store(true, Ordering::Release);
        let started = Instant::now();
        while self.snapshot().busy && started.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(100));
        }
        ensure!(
            !self.snapshot().busy,
            "Background restore is still stopping"
        );
        // No resources ever created: do not require a running Docker engine to exit.
        let saved = self.store.load()?;
        if saved.active.is_some() || saved.pending.is_some() || !saved.retired.is_empty() {
            self.stop_all()?;
        }
        self.logs.flush();
        self.disarm_watchdog();
        Ok(())
    }
    fn arm_watchdog(&self) -> Result<()> {
        *self.watchdog.lock().unwrap() = Some(Watchdog::arm(
            &self.store.owner,
            &self.docker,
            &self.session,
        )?);
        Ok(())
    }
    fn disarm_watchdog(&self) {
        if let Some(watchdog) = self.watchdog.lock().unwrap().take() {
            watchdog.disarm();
        }
    }
    fn write_config(&self, instance: &Instance, archive: Option<&std::path::Path>) -> Result<()> {
        let directory = self.store.directory(instance)?;
        fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        service_file(
            &directory,
            "bootstrap.sql",
            &format!(
                "CREATE USER IF NOT EXISTS 'sandbox'@'%' IDENTIFIED BY '{}';\nALTER USER 'sandbox'@'%' IDENTIFIED BY '{}';\nGRANT ALL PRIVILEGES ON *.* TO 'sandbox'@'%';\nALTER USER IF EXISTS 'root'@'localhost' IDENTIFIED BY '{}';\n",
                self.store.password, self.store.password, self.store.root_password
            ),
        )?;
        service_file(
            &directory,
            "mysql-client.cnf",
            &format!(
                "[client]\nuser=sandbox\npassword={}\nprotocol=TCP\nhost=127.0.0.1\n",
                self.store.password
            ),
        )?;
        service_file(
            &directory,
            "mysql-entry.sh",
            "set -eu\ninstall -m 600 -o mysql -g mysql /run/sandbox/bootstrap.sql /tmp/sandbox-bootstrap.sql\ninstall -m 600 -o mysql -g mysql /run/sandbox/mysql-client.cnf /tmp/sandbox-client.cnf\nexec /docker-entrypoint.sh --user=mysql --init-file=/tmp/sandbox-bootstrap.sql --event-scheduler=OFF --skip-replica-start --bind-address=0.0.0.0\n",
        )?;
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(self.store.password.as_bytes()));
        service_file(
            &directory,
            "ch-users.xml",
            &format!(
                "<clickhouse><users><default><networks replace=\"replace\"><ip>127.0.0.1</ip><ip>::1</ip></networks></default><sandbox><password_sha256_hex>{hash}</password_sha256_hex><networks><ip>::/0</ip></networks><profile>default</profile><quota>default</quota><access_management>1</access_management></sandbox></users></clickhouse>"
            ),
        )?;
        service_file(
            &directory,
            "ch-client.xml",
            &format!(
                "<config><user>sandbox</user><password>{}</password><host>127.0.0.1</host></config>",
                self.store.password
            ),
        )?;
        service_file(
            &directory,
            "ch-config.xml",
            "<clickhouse><listen_host>0.0.0.0</listen_host><logger><level>information</level><console>true</console><log remove=\"remove\"/><errorlog remove=\"remove\"/></logger><storage_configuration><disks><backups><type>local</type><path>/backups/</path></backups></disks></storage_configuration><backups><allowed_disk>backups</allowed_disk><allowed_path>/backups/</allowed_path></backups></clickhouse>",
        )?;
        service_file(
            &directory,
            "ch-entry.sh",
            "set -eu\nmkdir -p /backups\nchown clickhouse:clickhouse /backups\ninstall -m 644 /run/sandbox/ch-config.xml /etc/clickhouse-server/config.d/sandbox.xml\ninstall -m 644 /run/sandbox/ch-users.xml /etc/clickhouse-server/users.d/sandbox.xml\ninstall -m 600 -o clickhouse -g clickhouse /run/sandbox/ch-client.xml /tmp/sandbox-client.xml\nexec /entrypoint.sh\n",
        )?;
        let compose = compose_definition(&self.store, instance, archive, &self.session)?;
        service_file(
            &directory,
            "compose.json",
            &serde_json::to_string_pretty(&compose)?,
        )?;
        Ok(())
    }
}
fn free_port(preferred: u16) -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", preferred))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?
        .local_addr()?
        .port())
}
fn compose_definition(
    store: &Store,
    instance: &Instance,
    archive: Option<&std::path::Path>,
    session: &str,
) -> Result<Value> {
    let directory = store
        .directory(instance)?
        .display()
        .to_string()
        .replace('$', "$$");
    let labels = json!({OWNER_LABEL:store.owner,INSTANCE_LABEL:instance.id,SESSION_LABEL:session});
    let bind = json!({"type":"bind","source":directory,"target":"/run/sandbox","read_only":true});
    let mut ch_mounts = vec![
        bind.clone(),
        json!({"type":"volume","source":"clickhouse-data","target":"/var/lib/clickhouse"}),
    ];
    if let Some(path) = archive {
        ch_mounts.push(json!({"type":"bind","source":path.display().to_string().replace('$',"$$"),"target":"/backups/clickhouse.tar.zst","read_only":true}));
    }
    Ok(json!({"name":instance.project,"services":{
        "mysql":{"image":MYSQL_IMAGE,"container_name":store.container(instance,"mysql"),"user":"0:0","entrypoint":["/bin/bash","/run/sandbox/mysql-entry.sh"],"restart":"no","labels":labels,"mem_limit":"2g","stop_grace_period":"20s","ports":[format!("127.0.0.1:{}:3306",instance.ports[0])],"volumes":[bind,{"type":"volume","source":"mysql-data","target":"/var/lib/mysql"}],"logging":{"driver":"json-file","options":{"max-size":"10m","max-file":"3"}}},
        "clickhouse":{"image":CH_IMAGE,"container_name":store.container(instance,"clickhouse"),"user":"0:0","entrypoint":["/bin/bash","/run/sandbox/ch-entry.sh"],"restart":"no","labels":labels,"mem_limit":"3g","stop_grace_period":"20s","ports":[format!("127.0.0.1:{}:8123",instance.ports[1]),format!("127.0.0.1:{}:9000",instance.ports[2])],"volumes":ch_mounts,"logging":{"driver":"json-file","options":{"max-size":"10m","max-file":"3"}}}
    },"volumes":{"mysql-data":{"name":store.volume(instance,0),"labels":labels},"clickhouse-data":{"name":store.volume(instance,1),"labels":labels}},"networks":{"default":{"labels":labels}}}))
}
struct CountedReader<R> {
    inner: R,
    cancel: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
    reported: Instant,
    log: Arc<ProcessLog>,
}
impl<R: Read> Read for CountedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(std::io::Error::other("Cancelled"));
        }
        let n = self.inner.read(buf)?;
        let bytes = self.bytes.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        if self.reported.elapsed() > Duration::from_secs(1) {
            self.log.append(
                "MySQL restore",
                format!("Extracted {}", crate::backups::format_size(bytes)),
            );
            self.reported = Instant::now();
        }
        Ok(n)
    }
}

/// Runs before GPUI/logging initialization in a minimal child process. EOF is the
/// ownership signal, so PID reuse cannot accidentally keep a crashed session alive.
pub fn watchdog_mode() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_none_or(|arg| arg != "--sandbox-watchdog") {
        return false;
    }
    if args.len() != 5 {
        return true;
    }
    let owner = args[2].to_string_lossy();
    if !crate::sandbox_store::valid_id(&owner) {
        return true;
    }
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    if input == "clean" {
        return true;
    }
    let docker = Docker {
        binary: args[3].clone().into(),
    };
    let session = args[4].to_string_lossy();
    if !crate::sandbox_store::valid_id(&session) {
        return true;
    }
    for _ in 0..3 {
        if let Ok(ids) = docker.output(
            &strings(&[
                "ps",
                "-q",
                "--filter",
                &format!("label={OWNER_LABEL}={owner}"),
                "--filter",
                &format!("label={SESSION_LABEL}={session}"),
            ]),
            SHORT,
        ) {
            let mut args = strings(&["stop", "--timeout", "15"]);
            args.extend(ids.lines().filter(|id| !id.is_empty()).map(str::to_owned));
            if args.len() == 3 || docker.output(&args, Duration::from_secs(45)).is_ok() {
                break;
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
    true
}

#[cfg(test)]
mod tests;
