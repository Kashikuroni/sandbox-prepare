use super::*;
use crate::backup_cache::Cache;
use crate::{archive_check, backup_cache::Record, sandbox_store::valid_id};
use rusqlite::{Connection, params};

fn fixture() -> Result<(Arc<Manager>, std::path::PathBuf)> {
    let root = std::env::temp_dir().join(format!("sandbox-db-test-{}", crate::docker::nonce()?));
    fs::create_dir_all(&root)?;
    let manager = Manager::at(root.join("application.sqlite3"), false)?;
    let started = Instant::now();
    while manager.snapshot().busy {
        ensure!(
            started.elapsed() < Duration::from_secs(60),
            "Preflight timed out"
        );
        thread::sleep(Duration::from_millis(100));
    }
    Ok((manager, root))
}
fn instance(manager: &Manager) -> Result<Instance> {
    let id = crate::docker::nonce()?;
    Ok(Instance {
        project: format!("sp-{}-{}", &manager.store.owner[..12], &id[..12]),
        id,
        ports: [free_port(0)?, free_port(0)?, free_port(0)?],
        backups: ["ch/test".into(), "mysql/test".into()],
        created: chrono::Utc::now().to_rfc3339(),
        tables: [0; 2],
    })
}

#[test]
fn compose_is_pinned_separated_labelled_and_contains_no_credentials() -> Result<()> {
    let root = std::env::temp_dir().join(format!("sandbox-compose-{}", crate::docker::nonce()?));
    let store = Store::open(root.join("application.sqlite3"))?;
    let id = crate::docker::nonce()?;
    let instance = Instance {
        id: id.clone(),
        project: format!("sp-{}-{}", &store.owner[..12], &id[..12]),
        ports: [13306, 18123, 19000],
        backups: ["ch/x".into(), "mysql/x".into()],
        created: String::new(),
        tables: [0; 2],
    };
    let compose = compose_definition(&store, &instance, None, &crate::docker::nonce()?)?;
    let encoded = serde_json::to_string(&compose)?;
    assert!(!encoded.contains(&store.password));
    assert!(!encoded.contains(&store.root_password));
    assert_eq!(compose["volumes"].as_object().unwrap().len(), 2);
    for service in ["mysql", "clickhouse"] {
        assert_eq!(compose["services"][service]["restart"], "no");
        assert_eq!(
            compose["services"][service]["labels"][OWNER_LABEL],
            store.owner
        );
        assert!(
            compose["services"][service]["image"]
                .as_str()
                .unwrap()
                .contains("@sha256:")
        );
        assert!(compose["services"][service].get("environment").is_none());
    }
    let mut malicious = instance.clone();
    malicious.project = "foreign-project".into();
    assert!(store.directory(&malicious).is_err());
    assert!(!valid_id("../../unsafe"));
    assert!(valid_id(&id));
    let reopened = Store::open(root.join("application.sqlite3"))?;
    assert_eq!(reopened.password, store.password);
    fs::remove_dir_all(root)?;
    Ok(())
}

fn prepare_backups(manager: &Manager, source: &Instance, root: &std::path::Path) -> Result<()> {
    manager.pull()?;
    manager.write_config(source, None)?;
    manager.compose(source, &["create", "--pull", "never"])?;
    let mut initialize = manager.probe_args(source, "-u");
    let image_position = initialize.iter().position(|s| s == MYSQL_IMAGE).unwrap();
    initialize.truncate(image_position - 2); // remove --entrypoint id; retain owned labels
    initialize.extend(strings(&["--user","0:0","--mount",&format!("type=volume,source={},target=/var/lib/mysql",manager.store.volume(source,0)),"--entrypoint","/bin/bash",MYSQL_IMAGE,"-ec","chown mysql:mysql /var/lib/mysql; mysqld --initialize-insecure --user=mysql --datadir=/var/lib/mysql"]));
    manager.run(
        &initialize,
        None,
        "Fixture initialize",
        Duration::from_secs(180),
    )?;
    manager.compose(source, &["up", "-d", "--no-build", "--pull", "never"])?;
    manager.wait_ready(source)?;
    manager.sql(source,true,"CREATE DATABASE fixture; CREATE TABLE fixture.numbers (id INT PRIMARY KEY); INSERT INTO fixture.numbers VALUES (1),(2),(3);")?;
    manager.sql(source,false,"CREATE DATABASE fixture; CREATE TABLE fixture.numbers (id UInt64) ENGINE=MergeTree ORDER BY id; INSERT INTO fixture.numbers VALUES (1),(2),(3); BACKUP DATABASE fixture TO Disk('backups','clickhouse.tar.zst');")?;
    let backups = root.join("backups");
    let generation = "pair-123456-1-0";
    let directory = backups.join(generation);
    fs::create_dir_all(&directory)?;
    manager.docker.output(
        &strings(&[
            "cp",
            &format!(
                "{}:/backups/clickhouse.tar.zst",
                manager.store.container(source, "clickhouse")
            ),
            &directory.join(FILES[0]).display().to_string(),
        ]),
        SHORT,
    )?;
    let config_dir = manager.store.directory(source)?;
    service_file(
        &config_dir,
        "root-client.cnf",
        &format!(
            "[client]\nuser=sandbox\npassword={}\nhost=127.0.0.1\n",
            manager.store.password
        ),
    )?;
    let mut args = strings(&[
        "run",
        "--rm",
        "--name",
        &manager.store.container(source, "fixture-backup"),
        "--label",
        &format!("{OWNER_LABEL}={}", manager.store.owner),
        "--label",
        &format!("{INSTANCE_LABEL}={}", source.id),
        "--network",
        &format!("container:{}", manager.store.container(source, "mysql")),
        "--user",
        "0:0",
        "--mount",
        &format!(
            "type=volume,source={},target=/var/lib/mysql,readonly",
            manager.store.volume(source, 0)
        ),
        "--mount",
        &format!(
            "type=bind,source={},target=/run/sandbox,readonly",
            config_dir.display()
        ),
        "--entrypoint",
        "xtrabackup",
        XB_IMAGE,
    ]);
    args.extend(strings(&[
        "--defaults-extra-file=/run/sandbox/root-client.cnf",
        "--backup",
        "--stream=xbstream",
        "--datadir=/var/lib/mysql",
        "--target-dir=/tmp/fixture-backup",
    ]));
    let stderr = File::create(root.join("fixture-xtrabackup.log"))?;
    let mut child = manager
        .docker
        .command(&args)
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()?;
    let mut encoder =
        zstd::stream::write::Encoder::new(File::create(directory.join(FILES[1]))?, 1)?;
    std::io::copy(&mut child.stdout.take().unwrap(), &mut encoder)?;
    encoder.finish()?;
    ensure!(
        child.wait()?.success(),
        "Fixture XtraBackup failed: {}",
        fs::read_to_string(root.join("fixture-xtrabackup.log"))?
    );
    let (_, cancel) = tokio::sync::watch::channel(false);
    let records: Vec<Record> = (0..2)
        .map(|index| -> Result<Record> {
            let path = directory.join(FILES[index]);
            Ok(Record {
                key: format!("{}fixture", crate::backup_cache::SOURCES[index].1),
                size: fs::metadata(&path)?.len(),
                modified: "2026-11-11T00:00:00Z".into(),
                etag: None,
                sha256: archive_check::verify(&path, index, &cancel)?,
                local_modified: None,
                verification_error: None,
            })
        })
        .collect::<Result<_>>()?;
    let pair = crate::backup_cache::Pair {
        generation: generation.into(),
        origin: crate::backup_cache::Origin::new(&crate::backups::StorageConfig::default()),
        records: records.try_into().unwrap(),
    };
    let db = Connection::open(&manager.store.database)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS backup_pair(id INTEGER PRIMARY KEY,manifest TEXT NOT NULL);",
    )?;
    db.execute(
        "INSERT OR REPLACE INTO backup_pair VALUES(1,?1)",
        params![serde_json::to_string(&pair)?],
    )?;
    manager.stop_followers();
    manager.remove_instance(source)?;
    Ok(())
}

// All resources in this test have a fresh random owner and are removed on failure too.
struct Cleanup {
    manager: Arc<Manager>,
    instances: Vec<Instance>,
    root: std::path::PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.manager.cancel.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.manager.snapshot().busy && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        self.manager.stop_followers();
        self.manager.cancel.store(false, Ordering::Release);
        let _ = self.manager.delete();
        if let Ok(saved) = self.manager.store.load() {
            for instance in saved
                .active
                .iter()
                .chain(saved.pending.iter())
                .chain(saved.retired.iter())
            {
                let _ = self.manager.remove_instance(instance);
            }
        }
        for instance in &self.instances {
            let _ = self.manager.remove_instance(instance);
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
#[ignore = "Creates isolated Docker fixtures, restores real physical backups, and removes its own resources"]
fn docker_restores_both_databases_preserves_old_on_failure_and_restarts() -> Result<()> {
    let (manager, root) = fixture()?;
    ensure!(
        manager.snapshot().docker_ready,
        "Docker preflight failed: {:?}",
        manager.snapshot().error
    );
    let source = instance(&manager)?;
    let _cleanup = Cleanup {
        manager: manager.clone(),
        instances: vec![source.clone()],
        root: root.clone(),
    };
    prepare_backups(&manager, &source, &root)?;
    let generation = "pair-123456-1-0";
    let directory = root.join("backups").join(generation);
    eprintln!("Fixtures prepared; restoring both databases");
    manager.restore().with_context(|| {
        format!(
            "Process log tail: {}",
            manager
                .logs
                .snapshot()
                .iter()
                .rev()
                .take(25)
                .map(|line| line.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    let restored = manager.snapshot().active.unwrap();
    assert_eq!(
        manager
            .sql(&restored, true, "SELECT SUM(id) FROM fixture.numbers;")?
            .trim(),
        "6"
    );
    assert_eq!(
        manager
            .sql(&restored, false, "SELECT sum(id) FROM fixture.numbers")?
            .trim(),
        "6"
    );
    manager.stop()?;
    manager.start()?;
    assert_eq!(
        manager
            .sql(&restored, true, "SELECT COUNT(*) FROM fixture.numbers;")?
            .trim(),
        "3"
    );
    assert_eq!(
        manager
            .sql(&restored, false, "SELECT count() FROM fixture.numbers")?
            .trim(),
        "3"
    );
    let lease = Cache::for_database(manager.store.database.clone())?.lease()?;
    assert_eq!(lease.pair.generation, generation);
    assert!(
        Cache::for_database(manager.store.database.clone())?
            .lease()
            .is_err()
    );
    drop(lease);
    // A corrupt ClickHouse archive must not replace or delete the active databases.
    let ch_path = directory.join(FILES[0]);
    let mut corrupt = fs::read(&ch_path)?;
    corrupt[0] ^= 1;
    fs::write(&ch_path, corrupt)?;
    assert!(manager.restore().is_err());
    assert_eq!(manager.store.load()?.active.unwrap().id, restored.id);
    assert_eq!(
        manager
            .sql(&restored, true, "SELECT SUM(id) FROM fixture.numbers;")?
            .trim(),
        "6"
    );
    assert_eq!(
        manager
            .sql(&restored, false, "SELECT sum(id) FROM fixture.numbers")?
            .trim(),
        "6"
    );
    manager.delete()?;
    assert!(manager.store.load()?.active.is_none());
    Ok(())
}

#[test]
#[ignore = "Runs the built app watchdog against two disposable Docker containers"]
fn watchdog_stops_only_its_session_when_parent_pipe_closes() -> Result<()> {
    let docker = Docker::discover();
    let owner = crate::docker::nonce()?;
    let session = crate::docker::nonce()?;
    let other = crate::docker::nonce()?;
    let names = [
        format!("sp-watchdog-{}", &session[..12]),
        format!("sp-watchdog-{}", &other[..12]),
    ];
    struct Containers(Docker, [String; 2]);
    impl Drop for Containers {
        fn drop(&mut self) {
            for name in &self.1 {
                let _ = self.0.output(&strings(&["rm", "-f", "-v", name]), SHORT);
            }
        }
    }
    let _cleanup = Containers(docker.clone(), names.clone());
    for (index, session) in [&session, &other].into_iter().enumerate() {
        docker.output(
            &strings(&[
                "run",
                "-d",
                "--name",
                &names[index],
                "--network",
                "none",
                "--label",
                &format!("{OWNER_LABEL}={owner}"),
                "--label",
                &format!("{SESSION_LABEL}={session}"),
                "--entrypoint",
                "sleep",
                MYSQL_IMAGE,
                "120",
            ]),
            SHORT,
        )?;
    }
    let binary = std::env::current_exe()?
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sandbox-prepare");
    let mut child = Command::new(binary)
        .arg("--sandbox-watchdog")
        .arg(&owner)
        .arg(&docker.binary)
        .arg(&session)
        .stdin(Stdio::piped())
        .spawn()?;
    drop(child.stdin.take());
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "Watchdog failed");
            break;
        }
        if started.elapsed() > Duration::from_secs(50) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Watchdog did not exit");
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        docker
            .output(
                &strings(&["inspect", "--format", "{{.State.Running}}", &names[0]]),
                SHORT
            )?
            .trim(),
        "false"
    );
    assert_eq!(
        docker
            .output(
                &strings(&["inspect", "--format", "{{.State.Running}}", &names[1]]),
                SHORT
            )?
            .trim(),
        "true"
    );
    Ok(())
}

#[test]
#[ignore = "Restores two scoped sandboxes concurrently from disposable physical backups"]
fn two_sandboxes_share_archives_but_not_databases_or_project_processes() -> Result<()> {
    use sandbox_core::models::{
        ComposeLaunchSpec, DatabaseBinding, DatabaseId, DatabaseParameter, SourceMode,
        SourceSelection,
    };
    let (fixture, root) = fixture()?;
    let source = instance(&fixture)?;
    let _fixture_cleanup = Cleanup {
        manager: fixture.clone(),
        instances: vec![source.clone()],
        root: root.clone(),
    };
    prepare_backups(&fixture, &source, &root)?;
    let catalog = Catalog::open(root.join("application.sqlite3"))?;
    let project = catalog.projects()?.remove(0);
    let executor = tokio::runtime::Runtime::new()?;
    let operations = Operations::new(executor.handle().clone());
    let limit = RestoreLimit::new(2);
    let memory = Arc::new(sandbox_telemetry::process_log::Memory::default());
    let session = crate::docker::nonce()?;
    let directory = root.join("fixture-project");
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("compose.json"),
        serde_json::to_vec(
            &json!({"services":{"api":{"image":MYSQL_IMAGE,"entrypoint":["/bin/bash","-c"],"command":["echo project-started; exec sleep 600"],"depends_on":{"db":{"condition":"service_healthy"}},"volumes":["app-data:/app-data"]},"db":{"image":MYSQL_IMAGE}},"volumes":{"app-data":{}}}),
        )?,
    )?;
    let mut managers = vec![];
    let mut cleanup = vec![];
    for index in 0..2 {
        let mut sandbox =
            catalog.create_sandbox(project.id.clone(), format!("Parallel {index}"))?;
        sandbox.source = Some(SourceSelection {
            directory: directory.clone(),
            mode: SourceMode::CurrentCheckout,
            owned: false,
            revision: "fixture".into(),
        });
        let mut spec = ComposeLaunchSpec {
            files: vec!["compose.json".into()],
            services: vec!["api".into()],
            ..Default::default()
        };
        spec.databases.insert(
            "db".into(),
            DatabaseId::try_from("mysql".to_owned()).unwrap(),
        );
        spec.bindings.push(DatabaseBinding {
            service: "api".into(),
            variable: "DB_HOST".into(),
            database: DatabaseId::try_from("mysql".to_owned()).unwrap(),
            parameter: DatabaseParameter::Host,
            database_name: String::new(),
        });
        sandbox.compose = Some(spec);
        catalog.save_sandbox(&sandbox)?;
        let manager = Manager::scoped(
            catalog.clone(),
            sandbox,
            operations.clone(),
            limit.clone(),
            session.clone(),
            memory.clone(),
        )?;
        wait(&manager, Duration::from_secs(60))?;
        cleanup.push(Cleanup {
            manager: manager.clone(),
            instances: vec![],
            root: root.join(format!("unused-cleanup-{index}")),
        });
        managers.push(manager);
    }
    for manager in &managers {
        manager.launch(Action::Create);
    }
    for manager in &managers {
        wait(manager, Duration::from_secs(600))?;
    }
    let first = managers[0]
        .snapshot()
        .active
        .context("First sandbox did not restore")?;
    let second = managers[1]
        .snapshot()
        .active
        .context("Second sandbox did not restore")?;
    assert_ne!(first.ports, second.ports);
    assert_ne!(managers[0].store.password, managers[1].store.password);
    managers[0].sql(&first, true, "INSERT INTO fixture.numbers VALUES (99);")?;
    assert_eq!(
        managers[1]
            .sql(&second, true, "SELECT COUNT(*) FROM fixture.numbers;")?
            .trim(),
        "3"
    );
    for (manager, instance) in managers.iter().zip([&first, &second]) {
        assert_eq!(
            manager
                .sql(instance, false, "SELECT sum(id) FROM fixture.numbers")?
                .trim(),
            "6"
        );
        let app = format!("{}-app-api", instance.project);
        assert_eq!(
            manager
                .docker
                .output(
                    &strings(&["inspect", "--format", "{{.State.Running}}", &app]),
                    SHORT
                )?
                .trim(),
            "true"
        );
        let environment = manager.docker.output(
            &strings(&["inspect", "--format", "{{json .Config.Env}}", &app]),
            SHORT,
        )?;
        assert!(environment.contains("DB_HOST=db"));
    }
    managers[0].stop()?;
    assert_eq!(
        managers[1]
            .sql(&second, true, "SELECT COUNT(*) FROM fixture.numbers;")?
            .trim(),
        "3"
    );
    assert_eq!(
        managers[1]
            .docker
            .output(
                &strings(&[
                    "inspect",
                    "--format",
                    "{{.State.Running}}",
                    &format!("{}-app-api", second.project)
                ]),
                SHORT
            )?
            .trim(),
        "true"
    );
    operations.stop();
    executor.block_on(operations.join());
    for manager in &managers {
        manager.shutdown_blocking()?;
        assert!(manager.owned_containers()?.iter().all(|id| {
            manager
                .docker
                .output(
                    &strings(&["inspect", "--format", "{{.State.Running}}", id]),
                    SHORT,
                )
                .is_ok_and(|s| s.trim() == "false")
        }));
    }
    drop(cleanup);
    Ok(())
}
fn wait(manager: &Manager, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while manager.snapshot().busy {
        ensure!(
            started.elapsed() < timeout,
            "Sandbox timed out: {}",
            manager.snapshot().stage
        );
        thread::sleep(Duration::from_millis(100));
    }
    ensure!(
        manager.snapshot().error.is_none(),
        "Sandbox failed: {}",
        manager.snapshot().error.unwrap_or_default()
    );
    Ok(())
}
