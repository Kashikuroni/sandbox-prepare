//! Seed sandbox-owned worktrees with local environment files absent from Git.
use anyhow::{Context, Result, ensure};
use sandbox_core::models::{Project, Sandbox};
use sandbox_platform::catalog::Catalog;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

fn environment_name(name: &str, directory: bool) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with(".env")
        || if directory {
            matches!(name.as_str(), "env" | "envs")
        } else {
            name.ends_with(".env") || name.contains(".env.")
        }
}
fn excluded(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | ".venv"
            | "venv"
            | "vendor"
            | "dist"
            | "build"
            | ".cache"
            | "__pycache__"
    )
}
fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "Environment copy cancelled"
    );
    Ok(())
}
fn directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Environment destination is not a real directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return directory(path);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
struct Temporary(std::path::PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn copy_file(source: &Path, target: &Path, cancel: &AtomicBool) -> Result<()> {
    if fs::symlink_metadata(target).is_ok() {
        return Ok(());
    }
    let temporary = Temporary(target.with_file_name(format!(
        ".sandbox-env-{}",
        sandbox_platform::docker::nonce()?
    )));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temporary.0)?;
    let mut input = fs::File::open(source)?;
    let mut bytes = [0; 64 * 1024];
    loop {
        check(cancel)?;
        let count = input.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        output.write_all(&bytes[..count])?;
    }
    output.sync_all()?;
    check(cancel)?;
    // Publish the complete file atomically, without replacing a concurrently created file.
    match fs::hard_link(&temporary.0, target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
fn walk(
    source: &Path,
    target: &Path,
    root: &Path,
    selected: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    check(cancel)?;
    for entry in fs::read_dir(source)? {
        check(cancel)?;
        let entry = entry?;
        let kind = entry.file_type()?;
        // Never traverse source symlinks or copy sockets/devices.
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let label = name.to_string_lossy();
        if kind.is_dir() {
            if label == ".git"
                || (!selected && excluded(&label))
                || entry.path().join("pyvenv.cfg").is_file()
            {
                continue;
            }
            let selected = selected || environment_name(&label, true);
            if selected {
                ensure_parents(&target.join(&name), root)?;
            }
            // Only create destination directories that contain selected files.
            walk(&entry.path(), &target.join(&name), root, selected, cancel)?;
        } else if kind.is_file() && (selected || environment_name(&label, false)) {
            ensure_parents(target, root)?;
            copy_file(&entry.path(), &target.join(&name), cancel).with_context(|| {
                format!(
                    "Could not copy local environment file {}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}
fn ensure_parents(path: &Path, root: &Path) -> Result<()> {
    ensure!(
        path.starts_with(root),
        "Environment path escapes the worktree"
    );
    if path != root {
        ensure_parents(path.parent().context("Invalid environment parent")?, root)?;
    }
    directory(path)
}

pub(crate) fn prepare(
    catalog: &Catalog,
    project: &Project,
    sandbox: &Sandbox,
    cancel: &AtomicBool,
) -> Result<()> {
    let Some(source) = sandbox.source.as_ref().filter(|source| source.owned) else {
        return Ok(());
    };
    let expected = catalog.root.join("worktrees").join(sandbox.id.as_str());
    ensure!(expected.is_dir(), "Managed worktree is unavailable");
    directory(&expected)?;
    ensure!(
        source.directory.canonicalize()? == expected.canonicalize()?,
        "Refusing to seed an unowned worktree"
    );
    let main = &project
        .repository
        .as_ref()
        .context("Project repository is missing")?
        .directory;
    if main.canonicalize()? == source.directory.canonicalize()? {
        return Ok(());
    }
    walk(main, &expected, &expected, false, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_nested_env_files_preserves_existing_and_avoids_dependencies_and_symlinks()
    -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-env-copy-{}",
            sandbox_platform::docker::nonce()?
        ));
        fs::create_dir_all(&root)?;
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let source = root.join("source");
        let target = root.join("target");
        for name in [
            "envs/nested",
            ".envs",
            "services/api",
            "node_modules/pkg",
            ".venv",
            "env/lib",
        ] {
            fs::create_dir_all(source.join(name))?;
        }
        fs::create_dir_all(&target)?;
        for name in [
            ".env",
            ".env.local",
            ".envrc",
            "envs/nested/secret",
            ".envs/config",
            "services/api/prod.env",
            "services/api/local.env.test",
            "node_modules/pkg/.env",
            ".venv/.env",
            "env/lib/large",
        ] {
            fs::write(source.join(name), name)?;
        }
        fs::write(source.join("env/pyvenv.cfg"), "virtualenv")?;
        fs::write(source.join("services/api/main.rs"), "code")?;
        fs::write(target.join(".env"), "keep local edits")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, source.join("envs/loop"))?;
        let cancel = AtomicBool::new(false);
        walk(&source, &target, &target, false, &cancel)?;
        assert_eq!(fs::read_to_string(target.join(".env"))?, "keep local edits");
        for name in [
            ".env.local",
            ".envrc",
            "envs/nested/secret",
            ".envs/config",
            "services/api/prod.env",
            "services/api/local.env.test",
        ] {
            assert_eq!(fs::read_to_string(target.join(name))?, name);
        }
        assert!(!target.join("node_modules").exists());
        assert!(!target.join(".venv").exists());
        assert!(!target.join("env").exists());
        assert!(!target.join("services/api/main.rs").exists());
        assert!(!target.join("envs/loop").exists());
        fs::write(source.join(".env.local"), "new source value")?;
        fs::remove_file(target.join(".envrc"))?;
        walk(&source, &target, &target, false, &cancel)?;
        assert_eq!(fs::read_to_string(target.join(".env.local"))?, ".env.local");
        assert!(target.join(".envrc").is_file());
        cancel.store(true, Ordering::Release);
        assert!(walk(&source, &target, &target, false, &cancel).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(target.join(".env.local"))?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            let redirected = root.join("redirected");
            fs::create_dir_all(&redirected)?;
            fs::remove_dir_all(target.join("envs"))?;
            std::os::unix::fs::symlink(&redirected, target.join("envs"))?;
            cancel.store(false, Ordering::Release);
            assert!(walk(&source, &target, &target, false, &cancel).is_err());
            assert!(!redirected.join("nested/secret").exists());
        }
        Ok(())
    }
}
