//! Successful per-service inputs; Compose remains responsible for container reconciliation.
use super::*;
use sandbox_core::models::{ComposeLaunchSpec, MountRole};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Default, Serialize, Deserialize)]
pub(super) struct LaunchState {
    services: BTreeMap<String, Inputs>,
}
#[derive(Serialize, Deserialize)]
struct Inputs {
    configuration: String,
    build: Option<String>,
    sources: Option<String>,
    image: String,
    #[serde(default)]
    image_id: Option<String>,
    volatile_build: bool,
}
fn image_tag(image: &str) -> String {
    if image.contains('@')
        || image
            .rsplit('/')
            .next()
            .is_some_and(|part| part.contains(':'))
    {
        image.to_owned()
    } else {
        format!("{image}:latest")
    }
}
fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
/// One metadata pass per distinct context/mount, without rereading source contents.
fn tree(
    path: &Path,
    cancel: &AtomicBool,
    hash: &mut Sha256,
    excluded: &std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    if excluded.contains(path) {
        return Ok(());
    }
    ensure!(!cancel.load(Ordering::Acquire), "Cancelled");
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Launch input is missing: {}", path.display()))?;
    hash.update(serde_json::to_vec(&path.to_string_lossy())?);
    #[cfg(unix)]
    if metadata.is_dir() {
        use std::os::unix::fs::MetadataExt;
        hash.update(metadata.mode().to_le_bytes());
    }
    if !metadata.is_dir() {
        hash.update(metadata.len().to_le_bytes());
        if let Ok(time) = metadata.modified() {
            hash.update(
                time.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .to_le_bytes(),
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // ctime detects replacements/edits even when a tool preserves mtime and size.
            hash.update(metadata.ctime().to_le_bytes());
            hash.update(metadata.ctime_nsec().to_le_bytes());
            hash.update(metadata.ino().to_le_bytes());
            hash.update(metadata.mode().to_le_bytes());
        }
    }
    if metadata.file_type().is_symlink() {
        hash.update(fs::read_link(path)?.to_string_lossy().as_bytes());
    } else if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_name() != ".git" {
                tree(&entry.path(), cancel, hash, excluded)?;
            }
        }
    }
    Ok(())
}
fn stamp(
    path: &Path,
    cancel: &AtomicBool,
    cache: &mut BTreeMap<PathBuf, String>,
) -> Result<String> {
    let path = path
        .canonicalize()
        .with_context(|| format!("Launch input is missing: {}", path.display()))?;
    if let Some(value) = cache.get(&path) {
        return Ok(value.clone());
    }
    let mut hash = Sha256::new();
    tree(&path, cancel, &mut hash, &Default::default())?;
    let value = format!("{:x}", hash.finalize());
    cache.insert(path, value.clone());
    Ok(value)
}
// Prune only literal Docker exclusions with no negations. Complex patterns stay in the
// conservative metadata scan, avoiding a second, subtly different Dockerignore parser.
fn context_stamp(
    path: &Path,
    dockerfile: Option<&str>,
    cancel: &AtomicBool,
    cache: &mut BTreeMap<(PathBuf, PathBuf, PathBuf), String>,
) -> Result<String> {
    let path = path.canonicalize()?;
    let dockerfile = path.join(dockerfile.unwrap_or("Dockerfile"));
    let specific = PathBuf::from(format!("{}.dockerignore", dockerfile.display()));
    let ignore = if specific.is_file() {
        specific
    } else {
        path.join(".dockerignore")
    };
    let key = (path.clone(), dockerfile.clone(), ignore.clone());
    if let Some(value) = cache.get(&key) {
        return Ok(value.clone());
    }
    let mut excluded = std::collections::BTreeSet::new();
    let content = match fs::read_to_string(&ignore) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    if !content.lines().any(|line| line.trim().starts_with('!')) {
        for line in content.lines().map(str::trim) {
            let line = line.trim_matches('/');
            if line.is_empty() || line.starts_with('#') || line.contains(['*', '?', '[', '\\']) {
                continue;
            }
            let rule = Path::new(line);
            if rule
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
            {
                excluded.insert(path.join(rule));
            }
        }
    }
    // Docker always consumes the Dockerfile and ignore rules, even when excluded from COPY.
    excluded.remove(&dockerfile);
    excluded.remove(&ignore);
    let mut hash = Sha256::new();
    tree(&path, cancel, &mut hash, &excluded)?;
    if dockerfile.exists() {
        tree(&dockerfile, cancel, &mut hash, &Default::default())?;
    }
    if ignore.exists() {
        tree(&ignore, cancel, &mut hash, &Default::default())?;
    }
    let value = format!("{:x}", hash.finalize());
    cache.insert(key, value.clone());
    Ok(value)
}
impl LaunchState {
    fn capture(
        document: &Value,
        services: &[String],
        project: &str,
        spec: Option<&ComposeLaunchSpec>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut state = Self::default();
        let mut cache = BTreeMap::new();
        let mut contexts = BTreeMap::new();
        for name in services {
            let config = &document["services"][name];
            let mut build_hash = Sha256::new();
            let mut volatile = false;
            let build = config.get("build").filter(|v| !v.is_null());
            if let Some(build) = build {
                build_hash.update(serde_json::to_vec(build)?);
                build_hash.update(serde_json::to_vec(&config["image"])?);
                for context in std::iter::once(&build["context"]).chain(
                    build["additional_contexts"]
                        .as_object()
                        .into_iter()
                        .flat_map(|values| values.values()),
                ) {
                    if let Some(path) = context.as_str() {
                        if path.starts_with("service:") {
                            continue;
                        }
                        if Path::new(path).is_absolute() {
                            build_hash.update(context_stamp(
                                Path::new(path),
                                (Some(path) == build["context"].as_str())
                                    .then(|| build["dockerfile"].as_str())
                                    .flatten(),
                                cancel,
                                &mut contexts,
                            )?);
                        } else {
                            volatile = true;
                        }
                    }
                }
                if let Some(file) = build["dockerfile"].as_str() {
                    let file = Path::new(file);
                    let path = if file.is_absolute() {
                        file.to_path_buf()
                    } else {
                        Path::new(build["context"].as_str().unwrap_or("")).join(file)
                    };
                    if path.is_file() {
                        build_hash.update(stamp(&path, cancel, &mut cache)?);
                    }
                }
                // Credential/SSH-backed or remote build inputs cannot safely be cached locally.
                volatile |= build.get("ssh").is_some() || build["context"].as_str().is_none();
                for secret in build["secrets"].as_array().into_iter().flatten() {
                    let source = secret["source"]
                        .as_str()
                        .or_else(|| secret.as_str())
                        .unwrap_or("");
                    let definition = &document["secrets"][source];
                    if let Some(file) = definition["file"].as_str() {
                        build_hash.update(stamp(Path::new(file), cancel, &mut cache)?);
                    } else {
                        volatile = true;
                    }
                }
            }
            let mut sources = Vec::new();
            for mount in config["volumes"].as_array().into_iter().flatten() {
                let key = format!("{name}:{}", mount["target"].as_str().unwrap_or(""));
                if mount["type"] == "bind"
                    && spec.is_some_and(|spec| {
                        matches!(
                            spec.mounts.get(&key),
                            Some(MountRole::Source | MountRole::Configuration)
                        )
                    })
                {
                    if let Some(path) = mount["source"].as_str() {
                        sources.push(stamp(Path::new(path), cancel, &mut cache)?);
                    }
                }
            }
            state.services.insert(
                name.clone(),
                Inputs {
                    configuration: digest(serde_json::to_vec(config)?),
                    build: build.map(|_| format!("{:x}", build_hash.finalize())),
                    sources: (!sources.is_empty()).then(|| digest(sources.join("\n"))),
                    image: image_tag(
                        config["image"]
                            .as_str()
                            .unwrap_or(&format!("{project}-{name}")),
                    ),
                    image_id: None,
                    volatile_build: volatile,
                },
            );
        }
        Ok(state)
    }
    fn builds(
        &self,
        previous: &Self,
        images: &BTreeMap<String, String>,
        document: &Value,
    ) -> Vec<String> {
        let mut changed = std::collections::BTreeSet::new();
        for (name, input) in &self.services {
            if input.build.is_some()
                && (input.volatile_build
                    || previous.services.get(name).is_none_or(|old| {
                        old.build != input.build
                            || old.image_id.as_ref() != images.get(&input.image)
                    })
                    || !images.contains_key(&input.image))
            {
                changed.insert(name.clone());
            }
        }
        loop {
            let before = changed.len();
            for name in self.services.keys() {
                if document["services"][name]["build"]["additional_contexts"]
                    .as_object()
                    .into_iter()
                    .flat_map(|v| v.values())
                    .filter_map(Value::as_str)
                    .filter_map(|v| v.strip_prefix("service:"))
                    .any(|name| changed.contains(name))
                {
                    changed.insert(name.clone());
                }
            }
            if changed.len() == before {
                break;
            }
        }
        changed.into_iter().collect()
    }
    fn restarts(&self, previous: &Self, builds: &[String]) -> Vec<String> {
        self.services
            .iter()
            .filter(|(name, input)| {
                !builds.contains(name)
                    && previous.services.get(*name).is_some_and(|old| {
                        old.configuration == input.configuration && old.sources != input.sources
                    })
            })
            .map(|(name, _)| name.clone())
            .collect()
    }
}
impl Manager {
    fn local_images(&self) -> Result<BTreeMap<String, String>> {
        let output = self.docker.capture(
            &strings(&[
                "image",
                "ls",
                "--no-trunc",
                "--format",
                "{{.Repository}}:{{.Tag}} {{.ID}}",
            ]),
            &BTreeMap::new(),
            &self.cancel,
            SHORT,
        )?;
        Ok(output
            .lines()
            .filter_map(|line| {
                line.split_once(' ')
                    .map(|(name, id)| (name.to_owned(), id.to_owned()))
            })
            .collect())
    }
    pub(super) fn start_project(
        &self,
        instance: &Instance,
        plan: &sandbox_projects::compose::Plan,
        allow_build: bool,
    ) -> Result<()> {
        let directory = self.store.directory(instance)?;
        let path = directory.join("launch-state.json");
        let previous: LaunchState = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                self.logs.append(
                    "Lifecycle",
                    "Invalid launch cache; required builds will be checked again".into(),
                );
                LaunchState::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LaunchState::default(),
            Err(error) => return Err(error.into()),
        };
        let document = sandbox_projects::compose::inspect_environment(
            &directory,
            &ComposeLaunchSpec {
                files: vec!["compose.json".into()],
                ..Default::default()
            },
            &plan.environment,
            &self.cancel,
        )?;
        let spec = self
            .store
            .scope
            .as_ref()
            .map(|(catalog, id, _)| catalog.sandbox(id))
            .transpose()?
            .and_then(|sandbox| sandbox.compose);
        self.stage("Checking project changes…");
        let mut current = LaunchState::capture(
            &document,
            &plan.application_services,
            &instance.project,
            spec.as_ref(),
            &self.cancel,
        )?;
        let images = if current.services.values().any(|input| input.build.is_some()) {
            self.local_images()?
        } else {
            BTreeMap::new()
        };
        let builds = if allow_build {
            current.builds(&previous, &images, &document)
        } else {
            vec![]
        };
        if !builds.is_empty() {
            self.stage("Building changed project services…");
            let mut args = strings(&["build"]);
            args.extend(builds.clone());
            self.compose_command(instance, &args)?;
        }
        // A stopped container reads changed bind-mounted code/configuration on its first start.
        // Only already-running consumers need an additional restart after reconciliation.
        let mut restarts = current.restarts(&previous, &builds);
        if !restarts.is_empty() {
            let running = self.docker.capture(
                &strings(&[
                    "ps",
                    "--filter",
                    &format!("label={OWNER_LABEL}={}", self.store.owner),
                    "--filter",
                    &format!("label={INSTANCE_LABEL}={}", instance.id),
                    "--format",
                    "{{.Names}}",
                ]),
                &BTreeMap::new(),
                &self.cancel,
                SHORT,
            )?;
            restarts.retain(|name| {
                running
                    .lines()
                    .any(|container| container == format!("{}-app-{name}", instance.project))
            });
        }
        self.stage("Starting project services…");
        let mut args = strings(&["up", "-d", "--no-build", "--pull", "missing"]);
        args.extend(plan.application_services.clone());
        self.compose_command(instance, &args)?;
        if !restarts.is_empty() {
            self.stage("Restarting services with changed source/configuration mounts…");
            let mut args = strings(&["restart", "--no-deps"]);
            args.extend(restarts);
            self.compose_command(instance, &args)?;
        }
        self.follow(instance);
        self.wait_for_application(instance, plan)?;
        let images = if builds.is_empty() {
            images
        } else {
            self.local_images()?
        };
        for input in current.services.values_mut() {
            input.image_id = images.get(&input.image).cloned();
        }
        if allow_build {
            service_file(
                &directory,
                "launch-state.json",
                &serde_json::to_string(&current)?,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Result<Self> {
            let root = std::env::temp_dir()
                .join(format!("sandbox-launch-inputs-{}", crate::docker::nonce()?));
            for path in ["a", "b", "code", "data"] {
                fs::create_dir_all(root.join(path))?;
            }
            fs::write(root.join("a/source"), "original")?;
            fs::write(root.join("b/source"), "original")?;
            Ok(Self(root))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn capture(
        document: &Value,
        names: &[String],
        spec: Option<&ComposeLaunchSpec>,
    ) -> Result<LaunchState> {
        LaunchState::capture(document, names, "fixture", spec, &AtomicBool::new(false))
    }
    #[test]
    fn unchanged_builds_are_reused_and_only_changed_inputs_and_dependents_rebuild() -> Result<()> {
        let root = Fixture::new()?;
        let mut document = json!({"services":{
            "a":{"build":{"context":root.0.join("a")},"image":"fixture-a"},
            "b":{"build":{"context":root.0.join("b")},"image":"fixture-b"},
            "derived":{"build":{"context":root.0.join("b"),"additional_contexts":{"base":"service:a"}},"image":"fixture-derived"}
        }});
        let names = vec!["a".into(), "b".into(), "derived".into()];
        let images = BTreeMap::from([
            ("fixture-a:latest".into(), "sha256:a".into()),
            ("fixture-b:latest".into(), "sha256:b".into()),
            ("fixture-derived:latest".into(), "sha256:c".into()),
        ]);
        let mut previous = capture(&document, &names, None)?;
        assert_eq!(
            previous.builds(&LaunchState::default(), &images, &document),
            names
        );
        for input in previous.services.values_mut() {
            input.image_id = images.get(&input.image).cloned();
        }
        let persisted = serde_json::to_vec(&previous)?;
        let previous: LaunchState = serde_json::from_slice(&persisted)?;
        assert!(
            capture(&document, &names, None)?
                .builds(&previous, &images, &document)
                .is_empty()
        );
        document["services"]["b"]["environment"] = json!({"TOKEN":"changed-runtime-only"});
        assert!(
            capture(&document, &names, None)?
                .builds(&previous, &images, &document)
                .is_empty()
        );
        fs::write(root.0.join("a/source"), "updated source")?;
        assert_eq!(
            capture(&document, &names, None)?.builds(&previous, &images, &document),
            vec!["a", "derived"]
        );
        let mut missing = images.clone();
        missing.remove("fixture-b:latest");
        assert_eq!(
            capture(&document, &names, None)?.builds(&previous, &missing, &document),
            names
        );
        Ok(())
    }
    #[test]
    fn source_mount_changes_restart_only_the_consumer_and_data_mounts_do_not() -> Result<()> {
        let root = Fixture::new()?;
        fs::write(root.0.join("code/main"), "initial")?;
        let document = json!({"services":{"runner":{"image":"fixture","volumes":[
            {"type":"bind","source":root.0.join("code"),"target":"/code"},
            {"type":"bind","source":root.0.join("data"),"target":"/data"}
        ]}}});
        let spec = ComposeLaunchSpec {
            mounts: BTreeMap::from([
                ("runner:/code".into(), MountRole::Source),
                ("runner:/data".into(), MountRole::Data),
            ]),
            ..Default::default()
        };
        let names = vec!["runner".into()];
        let previous = capture(&document, &names, Some(&spec))?;
        fs::write(root.0.join("data/database"), "test data")?;
        assert!(
            capture(&document, &names, Some(&spec))?
                .restarts(&previous, &[])
                .is_empty()
        );
        fs::write(root.0.join("code/main"), "new code")?;
        assert_eq!(
            capture(&document, &names, Some(&spec))?.restarts(&previous, &[]),
            names
        );
        let mut changed_config = document.clone();
        changed_config["services"]["runner"]["command"] = json!("changed");
        assert!(
            capture(&changed_config, &names, Some(&spec))?
                .restarts(&previous, &[])
                .is_empty()
        );
        let serialized = String::from_utf8(serde_json::to_vec(&previous)?)?;
        assert!(!serialized.contains("initial"));
        Ok(())
    }
    #[test]
    fn literal_docker_exclusions_do_not_scan_or_invalidate_large_build_outputs() -> Result<()> {
        let root = Fixture::new()?;
        let context = root.0.join("a");
        fs::create_dir_all(context.join("target/debug"))?;
        fs::write(context.join(".dockerignore"), "target\n")?;
        fs::write(context.join("target/debug/output"), "initial build output")?;
        let cancel = AtomicBool::new(false);
        let before = context_stamp(&context, None, &cancel, &mut BTreeMap::new())?;
        fs::write(
            context.join("target/debug/output"),
            "new ignored build output",
        )?;
        fs::write(context.join("target/another"), "also ignored")?;
        assert_eq!(
            before,
            context_stamp(&context, None, &cancel, &mut BTreeMap::new())?
        );
        fs::write(context.join(".dockerignore"), "target\n!target/another\n")?;
        assert_ne!(
            before,
            context_stamp(&context, None, &cancel, &mut BTreeMap::new())?
        );
        fs::write(context.join("Dockerfile.dockerignore"), "source\n")?;
        let specific = context_stamp(&context, Some("Dockerfile"), &cancel, &mut BTreeMap::new())?;
        fs::write(
            context.join("source"),
            "ignored by Dockerfile-specific rules",
        )?;
        assert_eq!(
            specific,
            context_stamp(&context, Some("Dockerfile"), &cancel, &mut BTreeMap::new())?
        );
        Ok(())
    }

    #[test]
    fn remote_builds_are_conservative_and_cancelled_scans_stop() -> Result<()> {
        let doc = json!({"services":{"remote":{"build":{"context":"https://example.invalid/repository.git"}}}});
        let names = vec!["remote".into()];
        let state = capture(&doc, &names, None)?;
        let images = BTreeMap::from([("fixture-remote:latest".into(), "sha256:present".into())]);
        assert_eq!(state.builds(&state, &images, &doc), names);
        let root = Fixture::new()?;
        assert!(stamp(&root.0, &AtomicBool::new(true), &mut BTreeMap::new()).is_err());
        assert_eq!(
            image_tag("registry:5000/path/image"),
            "registry:5000/path/image:latest"
        );
        Ok(())
    }
    #[test]
    #[ignore = "Builds two disposable Docker services and verifies container reuse and selective rebuild"]
    fn docker_reuses_stopped_containers_and_rebuilds_only_changed_service() -> Result<()> {
        let root = Fixture::new()?;
        let catalog = Catalog::open(root.0.join("application.sqlite3"))?;
        let project = catalog.projects()?.remove(0);
        let mut sandbox =
            catalog.create_sandbox(project.id.clone(), "Fast launch fixture".into())?;
        sandbox.compose = Some(ComposeLaunchSpec::default());
        catalog.save_sandbox(&sandbox)?;
        let executor = tokio::runtime::Runtime::new()?;
        let operations = Operations::new(executor.handle().clone());
        let manager = Manager::scoped(
            catalog.clone(),
            sandbox.clone(),
            operations.clone(),
            RestoreLimit::new(1),
            crate::docker::nonce()?,
            Arc::new(sandbox_telemetry::process_log::Memory::default()),
        )?;
        let deadline = Instant::now() + Duration::from_secs(60);
        while manager.snapshot().busy {
            ensure!(Instant::now() < deadline, "Preflight timeout");
            thread::sleep(Duration::from_millis(50));
        }
        ensure!(manager.snapshot().docker_ready, "Docker is unavailable");
        let id = crate::docker::nonce()?;
        let instance = Instance {
            id: id.clone(),
            project: format!("sp-{}-{}", &catalog.owner[..12], &id[..12]),
            ports: [0; 3],
            dynamic_ports: false,
            backups: Default::default(),
            backup_set: None,
            created: String::new(),
            tables: [0; 2],
        };
        manager.store.save(&crate::sandbox_store::Saved {
            active: Some(instance.clone()),
            ..Default::default()
        })?;
        struct DockerCleanup {
            manager: Arc<Manager>,
            project: String,
        }
        impl Drop for DockerCleanup {
            fn drop(&mut self) {
                self.manager.stop_followers();
                let ids = self.manager.owned_containers().unwrap_or_default();
                if !ids.is_empty() {
                    let mut args = strings(&["rm", "-f"]);
                    args.extend(ids);
                    let _ = self.manager.docker.output(&args, SHORT);
                }
                let _ = self.manager.docker.output(
                    &strings(&["network", "rm", &format!("{}_default", self.project)]),
                    SHORT,
                );
                if let Ok(images) = self.manager.docker.output(
                    &strings(&[
                        "image",
                        "ls",
                        "-q",
                        "--filter",
                        &format!("label=com.docker.compose.project={}", self.project),
                    ]),
                    SHORT,
                ) {
                    let ids: std::collections::BTreeSet<_> =
                        images.lines().filter(|id| !id.is_empty()).collect();
                    if !ids.is_empty() {
                        let mut args = strings(&["image", "rm", "-f"]);
                        args.extend(ids.into_iter().map(str::to_owned));
                        let _ = self.manager.docker.output(&args, SHORT);
                    }
                }
            }
        }
        let _cleanup = DockerCleanup {
            manager: manager.clone(),
            project: instance.project.clone(),
        };
        let mut services = serde_json::Map::new();
        for name in ["a", "b"] {
            let context = root.0.join(name);
            fs::write(
                context.join("Dockerfile"),
                format!("FROM {MYSQL_IMAGE}\nCOPY source /fixture-source\n"),
            )?;
            services.insert(name.into(), json!({"image":format!("{}-{name}:latest",instance.project),"build":{"context":context},"container_name":format!("{}-app-{name}",instance.project),"entrypoint":["/bin/bash","-c"],"command":["exec sleep 600"],"restart":"no","labels":{OWNER_LABEL:catalog.owner,INSTANCE_LABEL:instance.id,SESSION_LABEL:manager.session}}));
        }
        services
            .get_mut("a")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("image");
        let plan = sandbox_projects::compose::Plan {
            document: json!({"name":instance.project,"services":services}),
            environment: Default::default(),
            application_services: vec!["a".into(), "b".into()],
            managed_services: Default::default(),
            connection_databases: Default::default(),
            readiness_probe: None,
        };
        catalog.save_generation_plan(&sandbox.id, &instance.id, &plan)?;
        let directory = manager.store.directory(&instance)?;
        fs::create_dir_all(&directory)?;
        service_file(
            &directory,
            "compose.json",
            &serde_json::to_string(&plan.document)?,
        )?;
        let identity = |name: &str| -> Result<String> {
            manager.docker.output(
                &strings(&[
                    "inspect",
                    "--format",
                    "{{.Id}}",
                    &format!("{}-app-{name}", instance.project),
                ]),
                SHORT,
            )
        };
        manager.start_project(&instance, &plan, true)?;
        let cached: LaunchState =
            serde_json::from_slice(&fs::read(directory.join("launch-state.json"))?)?;
        assert!(
            cached
                .services
                .values()
                .all(|input| input.image_id.is_some()),
            "Built image identities must be cached, including Compose default image names"
        );
        let first_a = identity("a")?;
        let first_b = identity("b")?;
        manager.stop()?;
        manager.start_project(&instance, &plan, true)?;
        assert_eq!(identity("a")?, first_a);
        assert_eq!(identity("b")?, first_b);
        fs::write(root.0.join("a/source"), "changed image content")?;
        manager.start_project(&instance, &plan, true)?;
        assert_ne!(identity("a")?, first_a);
        assert_eq!(identity("b")?, first_b);
        manager.stop()?;
        operations.stop();
        executor.block_on(operations.join());
        Ok(())
    }
}
