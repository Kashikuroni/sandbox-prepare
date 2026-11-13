//! Optional foreground preparation, cached only after successful commands and outputs.
use anyhow::{Context, Result, ensure};
use sandbox_core::models::{PreparationStep, Sandbox};
use sandbox_platform::{
    catalog::Catalog,
    docker::{Docker, private_file, strings},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

fn relative(root: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|p| matches!(p, Component::Normal(_) | Component::CurDir)),
        "Preparation paths must stay inside the working copy"
    );
    Ok(root.join(path))
}
fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Preparation cancelled");
    Ok(())
}
fn hash_path(
    path: &Path,
    ignored: &BTreeSet<PathBuf>,
    cancel: &AtomicBool,
    hash: &mut Sha256,
) -> Result<()> {
    check(cancel)?;
    if ignored.contains(path) {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Preparation input is missing: {}", path.display()))?;
    hash.update(serde_json::to_vec(&path.to_string_lossy())?);
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !matches!(
                entry.file_name().to_str(),
                Some(".git" | "node_modules" | "target" | ".venv" | "__pycache__" | ".cache")
            ) {
                hash_path(&entry.path(), ignored, cancel, hash)?;
            }
        }
    } else {
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
            hash.update(metadata.ctime().to_le_bytes());
            hash.update(metadata.ctime_nsec().to_le_bytes());
            hash.update(metadata.ino().to_le_bytes());
            hash.update(metadata.mode().to_le_bytes());
        }
        if metadata.file_type().is_symlink() {
            hash.update(fs::read_link(path)?.to_string_lossy().as_bytes());
        }
    }
    Ok(())
}
fn fingerprint(
    paths: &[PathBuf],
    ignored: &BTreeSet<PathBuf>,
    cancel: &AtomicBool,
    seed: &[u8],
) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(seed);
    for path in paths {
        hash_path(path, ignored, cancel, &mut hash)?;
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn outputs_exist(paths: &[PathBuf]) -> bool {
    !paths.is_empty() && paths.iter().all(|path| path.exists())
}
fn working_directory(sandbox: &Sandbox, step: &PreparationStep) -> Result<PathBuf> {
    let root = &sandbox
        .source
        .as_ref()
        .context("Choose a working copy first")?
        .directory;
    let directory = relative(root, &step.directory)?
        .canonicalize()
        .context("Preparation working directory is missing")?;
    ensure!(
        directory.starts_with(root.canonicalize()?),
        "Preparation working directory escapes the working copy"
    );
    Ok(directory)
}
pub fn validate(sandbox: &Sandbox, step: &PreparationStep) -> Result<()> {
    ensure!(
        !step.command.trim().is_empty(),
        "Enter a preparation command"
    );
    ensure!(!step.command.contains('\0'), "Invalid preparation command");
    let directory = working_directory(sandbox, step)?;
    for path in step.inputs.iter().chain(&step.outputs) {
        relative(&directory, path)?;
    }
    Ok(())
}
pub fn run(
    catalog: &Catalog,
    sandbox: &Sandbox,
    cancel: &AtomicBool,
    log: impl Fn(String) + Send + Sync,
) -> Result<()> {
    let Some(step) = sandbox
        .compose
        .as_ref()
        .and_then(|spec| spec.preparation.as_ref())
    else {
        return Ok(());
    };
    validate(sandbox, step)?;
    check(cancel)?;
    let directory = working_directory(sandbox, step)?;
    let inputs = step
        .inputs
        .iter()
        .map(|path| relative(&directory, path))
        .collect::<Result<Vec<_>>>()?;
    let outputs = step
        .outputs
        .iter()
        .map(|path| relative(&directory, path))
        .collect::<Result<Vec<_>>>()?;
    let ignored = outputs.iter().cloned().collect();
    let environment = catalog.environment(&sandbox.project_id, "*")?;
    let seed = serde_json::to_vec(&(step, &environment))?;
    let before = fingerprint(&inputs, &ignored, cancel, &seed)?;
    let cache = sandbox.root.join("preparation.json");
    let can_cache = !inputs.is_empty() && !outputs.is_empty();
    if can_cache && outputs_exist(&outputs) {
        let output = fingerprint(&outputs, &BTreeSet::new(), cancel, &[])?;
        if fs::read(&cache)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<(String, String)>(&bytes).ok())
            .is_some_and(|old| old == (before.clone(), output))
        {
            log("Preparation is up to date".into());
            return Ok(());
        }
    }
    // Never retain an old successful marker after a failed or cancelled command.
    match fs::remove_file(&cache) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    log("Running project preparation…".into());
    let runner = Docker {
        binary: if cfg!(windows) {
            "cmd.exe".into()
        } else {
            "/bin/sh".into()
        },
    };
    let args = if cfg!(windows) {
        strings(&["/D", "/S", "/C", &step.command])
    } else {
        strings(&["-ec", &step.command])
    };
    runner.run_with_env_in(
        &args,
        None,
        cancel,
        Duration::from_secs(3600),
        &environment,
        Some(&directory),
        &log,
    )?;
    check(cancel)?;
    for output in &outputs {
        ensure!(
            output.exists(),
            "Preparation succeeded but expected output is missing: {}",
            output.display()
        );
    }
    if can_cache && fingerprint(&inputs, &ignored, cancel, &seed)? == before {
        fs::create_dir_all(&sandbox.root)?;
        private_file(
            &cache,
            &serde_json::to_vec(&(
                before,
                fingerprint(&outputs, &BTreeSet::new(), cancel, &[])?,
            ))?,
        )?;
    } else if can_cache {
        log("Preparation inputs changed while running; next start will check them again".into());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use sandbox_core::models::{ComposeLaunchSpec, SourceMode, SourceSelection};
    fn fixture() -> Result<(Catalog, Sandbox, PathBuf)> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-preparation-{}",
            sandbox_platform::docker::nonce()?
        ));
        fs::create_dir_all(root.join("project"))?;
        let catalog = Catalog::open(root.join("data/application.sqlite3"))?;
        let project = catalog.projects()?.remove(0);
        let mut sandbox = catalog.create_sandbox(project.id, "Preparation".into())?;
        sandbox.source = Some(SourceSelection {
            directory: root.join("project"),
            mode: SourceMode::CurrentCheckout,
            owned: false,
            revision: "fixture".into(),
        });
        sandbox.compose = Some(ComposeLaunchSpec {
            preparation: Some(PreparationStep {
                command: "mkdir -p output; cp input output/result; printf x >> runs".into(),
                directory: ".".into(),
                inputs: vec!["input".into()],
                outputs: vec!["output/result".into()],
            }),
            ..Default::default()
        });
        fs::write(root.join("project/input"), "original")?;
        Ok((catalog, sandbox, root))
    }
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn preparation_reuses_outputs_rebuilds_changes_and_never_caches_failure() -> Result<()> {
        let (catalog, mut sandbox, root) = fixture()?;
        let _cleanup = Cleanup(root.clone());
        let cancel = AtomicBool::new(false);
        run(&catalog, &sandbox, &cancel, |_| {})?;
        run(&catalog, &sandbox, &cancel, |_| {})?;
        assert_eq!(fs::read_to_string(root.join("project/runs"))?, "x");
        fs::write(root.join("project/input"), "modified")?;
        run(&catalog, &sandbox, &cancel, |_| {})?;
        assert_eq!(
            fs::read_to_string(root.join("project/output/result"))?,
            "modified"
        );
        fs::remove_file(root.join("project/output/result"))?;
        run(&catalog, &sandbox, &cancel, |_| {})?;
        assert_eq!(fs::read_to_string(root.join("project/runs"))?, "xxx");
        sandbox
            .compose
            .as_mut()
            .unwrap()
            .preparation
            .as_mut()
            .unwrap()
            .command = "echo preparation-failed >&2; exit 7".into();
        assert!(run(&catalog, &sandbox, &cancel, |_| {}).is_err());
        assert!(!sandbox.root.join("preparation.json").exists());
        sandbox
            .compose
            .as_mut()
            .unwrap()
            .preparation
            .as_mut()
            .unwrap()
            .command = "true".into();
        fs::remove_file(root.join("project/output/result"))?;
        assert!(
            run(&catalog, &sandbox, &cancel, |_| {})
                .unwrap_err()
                .to_string()
                .contains("expected output is missing")
        );
        sandbox
            .compose
            .as_mut()
            .unwrap()
            .preparation
            .as_mut()
            .unwrap()
            .directory = "../escape".into();
        assert!(
            validate(
                &sandbox,
                sandbox
                    .compose
                    .as_ref()
                    .unwrap()
                    .preparation
                    .as_ref()
                    .unwrap()
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn cancellation_terminates_the_preparation_process_group() -> Result<()> {
        let (catalog, mut sandbox, root) = fixture()?;
        let _cleanup = Cleanup(root.clone());
        sandbox
            .compose
            .as_mut()
            .unwrap()
            .preparation
            .as_mut()
            .unwrap()
            .command = "sleep 30 & echo $! > child.pid; wait".into();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            signal.store(true, Ordering::Release);
        });
        let started = std::time::Instant::now();
        assert!(run(&catalog, &sandbox, &cancel, |_| {}).is_err());
        trigger.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_reaped(&root.join("project/child.pid"))?;
        assert!(!sandbox.root.join("preparation.json").exists());
        Ok(())
    }
    #[test]
    fn background_children_are_reaped_when_foreground_command_finishes() -> Result<()> {
        let (catalog, mut sandbox, root) = fixture()?;
        let _cleanup = Cleanup(root.clone());
        sandbox
            .compose
            .as_mut()
            .unwrap()
            .preparation
            .as_mut()
            .unwrap()
            .command =
            "sleep 30 & echo $! > child.pid; mkdir -p output; cp input output/result".into();
        let started = std::time::Instant::now();
        run(&catalog, &sandbox, &AtomicBool::new(false), |_| {})?;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_reaped(&root.join("project/child.pid"))?;
        Ok(())
    }
    fn assert_reaped(path: &Path) -> Result<()> {
        let pid = fs::read_to_string(path)?.trim().parse::<u32>()?.to_string();
        for _ in 0..40 {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(std::process::Stdio::null())
                .status()?
                .success()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        anyhow::bail!("Preparation child process was left running")
    }
    #[test]
    #[ignore = "Requires Docker Compose CLI; validates only disposable files, starts no containers"]
    fn preparation_creates_missing_mount_before_compose_launch() -> Result<()> {
        use sandbox_core::models::MountRole;
        let (catalog, mut sandbox, root) = fixture()?;
        let _cleanup = Cleanup(root.clone());
        fs::write(root.join("project/compose.json"), br#"{"services":{"api":{"image":"alpine:3.21","volumes":["./frontend/nginx_dist:/srv:ro"]}}}"#)?;
        let spec = sandbox.compose.as_mut().unwrap();
        spec.files = vec!["compose.json".into()];
        spec.mounts
            .insert("api:/srv".into(), MountRole::Configuration);
        spec.preparation = Some(PreparationStep {
            command: "mkdir -p frontend/nginx_dist; printf ready > frontend/nginx_dist/index.html"
                .into(),
            directory: ".".into(),
            inputs: vec!["input".into()],
            outputs: vec!["frontend/nginx_dist/index.html".into()],
        });
        let cancel = AtomicBool::new(false);
        assert!(
            crate::compose::validate_mounts(&catalog, &sandbox, &cancel)
                .unwrap_err()
                .to_string()
                .contains("api:/srv")
        );
        run(&catalog, &sandbox, &cancel, |_| {})?;
        crate::compose::validate_mounts(&catalog, &sandbox, &cancel)?;
        Ok(())
    }
}
