use anyhow::{Context, Result, ensure};
use sandbox_core::models::{Project, Repository, Sandbox, SourceMode, SourceSelection};
use sandbox_platform::{
    catalog::Catalog,
    docker::{Docker, strings},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct Worktree {
    pub directory: PathBuf,
    pub branch: Option<String>,
    pub revision: String,
    pub locked: bool,
    pub prunable: bool,
}
#[derive(Clone)]
pub struct Git {
    runner: Docker,
}
impl Default for Git {
    fn default() -> Self {
        Self {
            runner: Docker {
                binary: PathBuf::from("git"),
            },
        }
    }
}
impl Git {
    pub(crate) fn run(
        &self,
        directory: &Path,
        args: &[String],
        cancel: &AtomicBool,
    ) -> Result<String> {
        let mut command = strings(&["-C"]);
        command.push(directory.display().to_string());
        command.extend_from_slice(args);
        self.runner
            .capture(&command, &BTreeMap::new(), cancel, Duration::from_secs(60))
            .context("Git operation failed")
    }
    pub fn repository(&self, path: &Path, cancel: &AtomicBool) -> Result<Repository> {
        let root = self.run(path, &strings(&["rev-parse", "--show-toplevel"]), cancel)?;
        let directory = PathBuf::from(root.trim()).canonicalize()?;
        let worktrees = self.worktrees(&directory, cancel)?;
        let main = worktrees
            .first()
            .context("Repository has no working tree")?;
        Ok(Repository {
            directory: main.directory.clone(),
            owned: false,
            url: None,
        })
    }
    pub fn clone_repository(
        &self,
        url: &str,
        target: &Path,
        cancel: &AtomicBool,
        log: impl Fn(String) + Send + Sync,
    ) -> Result<Repository> {
        ensure!(!url.trim().is_empty(), "Enter a repository URL");
        ensure!(!target.exists(), "Clone directory already exists");
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        sandbox_telemetry::logging::register_credentials(url, "");
        self.runner.run(
            &[
                "clone".into(),
                "--".into(),
                url.into(),
                target.display().to_string(),
            ],
            None,
            cancel,
            Duration::from_secs(3600),
            log,
        )?;
        let mut repository = self.repository(target, cancel)?;
        repository.owned = true;
        repository.url = Some(url.into());
        Ok(repository)
    }
    pub fn worktrees(&self, root: &Path, cancel: &AtomicBool) -> Result<Vec<Worktree>> {
        let output = self.run(
            root,
            &strings(&["worktree", "list", "--porcelain", "-z"]),
            cancel,
        )?;
        let mut result = Vec::new();
        let mut current: Option<Worktree> = None;
        for field in output.split('\0') {
            if let Some(path) = field.strip_prefix("worktree ") {
                if let Some(tree) = current.take() {
                    result.push(tree);
                }
                current = Some(Worktree {
                    directory: PathBuf::from(path),
                    branch: None,
                    revision: String::new(),
                    locked: false,
                    prunable: false,
                });
            } else if let Some(tree) = &mut current {
                if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                    tree.branch = Some(branch.into());
                }
                if let Some(revision) = field.strip_prefix("HEAD ") {
                    tree.revision = revision.into();
                }
                if field.starts_with("locked") {
                    tree.locked = true;
                }
                if field.starts_with("prunable") {
                    tree.prunable = true;
                }
            }
        }
        if let Some(tree) = current {
            result.push(tree);
        }
        Ok(result)
    }
    pub fn branches(&self, root: &Path, cancel: &AtomicBool) -> Result<Vec<String>> {
        let output = self.run(
            root,
            &strings(&[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads",
                "refs/remotes",
            ]),
            cancel,
        )?;
        Ok(output
            .lines()
            .filter(|line| !line.ends_with("/HEAD"))
            .map(str::to_owned)
            .collect())
    }
    pub fn local_branches(&self, root: &Path, cancel: &AtomicBool) -> Result<Vec<String>> {
        Ok(self
            .run(
                root,
                &strings(&["for-each-ref", "--format=%(refname:short)", "refs/heads"]),
                cancel,
            )?
            .lines()
            .map(str::to_owned)
            .collect())
    }
    pub fn prepare(
        &self,
        catalog: &Catalog,
        project: &Project,
        sandbox: &mut Sandbox,
        mode: SourceMode,
        cancel: &AtomicBool,
    ) -> Result<SourceSelection> {
        self.prepare_from(catalog, project, sandbox, mode, None, cancel)
    }
    /// A new branch starts at the selected working copy, without switching its checkout.
    pub fn prepare_from(
        &self,
        catalog: &Catalog,
        project: &Project,
        sandbox: &mut Sandbox,
        mode: SourceMode,
        base_revision: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<SourceSelection> {
        let repository = project
            .repository
            .as_ref()
            .context("Choose the project's Git directory first")?;
        let trees = self.worktrees(&repository.directory, cancel)?;
        let main = trees.first().context("Main checkout is missing")?;
        let (directory, owned, revision) = match &mode {
            SourceMode::CurrentCheckout => (
                main.directory.clone(),
                false,
                self.run(&main.directory, &strings(&["rev-parse", "HEAD"]), cancel)?
                    .trim()
                    .to_owned(),
            ),
            SourceMode::ExistingWorktree { directory } => {
                let path = directory
                    .canonicalize()
                    .context("Worktree directory is unavailable")?;
                let tree = trees
                    .iter()
                    .find(|tree| tree.directory.canonicalize().ok().as_ref() == Some(&path))
                    .context("Worktree does not belong to this repository")?;
                ensure!(!tree.prunable, "Worktree is unavailable");
                (path, false, tree.revision.clone())
            }
            SourceMode::ManagedWorktree { branch } => {
                let branch = branch.trim();
                ensure!(!branch.is_empty(), "Enter a branch name");
                let branches = self.branches(&repository.directory, cancel)?;
                let remote = branches.iter().any(|name| name == branch)
                    && !self
                        .run(
                            &repository.directory,
                            &strings(&["show-ref", "--verify", &format!("refs/heads/{branch}")]),
                            cancel,
                        )
                        .is_ok();
                let local = if remote {
                    branch.split_once('/').context("Invalid remote branch")?.1
                } else {
                    branch
                };
                self.run(
                    &repository.directory,
                    &strings(&["check-ref-format", "--branch", local]),
                    cancel,
                )?;
                if let Some(tree) = trees
                    .iter()
                    .find(|tree| tree.branch.as_deref() == Some(local))
                {
                    anyhow::bail!(
                        "Branch is already checked out at {}. Choose Current checkout or Existing worktree.",
                        tree.directory.display()
                    );
                }
                let target = catalog.root.join("worktrees").join(sandbox.id.as_str());
                ensure!(
                    !target.exists(),
                    "Worktree directory already exists; select it as an existing worktree"
                );
                let base = base_revision.unwrap_or("HEAD");
                ensure!(
                    base == "HEAD"
                        || (matches!(base.len(), 40 | 64)
                            && base.bytes().all(|b| b.is_ascii_hexdigit())),
                    "Invalid source revision"
                );
                let revision = self
                    .run(
                        &main.directory,
                        &strings(&["rev-parse", "--verify", &format!("{base}^{{commit}}")]),
                        cancel,
                    )?
                    .trim()
                    .to_owned();
                // Persist ownership before Git creates anything; interrupted creation is visible.
                sandbox.source = Some(SourceSelection {
                    mode: mode.clone(),
                    directory: target.clone(),
                    owned: true,
                    revision: revision.clone(),
                });
                catalog.save_sandbox(sandbox)?;
                fs::create_dir_all(target.parent().unwrap())?;
                let mut args = strings(&["worktree", "add"]);
                if remote {
                    args.extend(strings(&["-b", local, "--track"]));
                    args.push(target.display().to_string());
                    args.push(branch.into());
                } else if branches.iter().any(|name| name == branch) {
                    args.push(target.display().to_string());
                    args.push(branch.into());
                } else {
                    args.extend(strings(&["-b", branch]));
                    args.push(target.display().to_string());
                    args.push(revision);
                }
                self.run(&repository.directory, &args, cancel)?;
                let revision = self
                    .run(&target, &strings(&["rev-parse", "HEAD"]), cancel)?
                    .trim()
                    .to_owned();
                (target.canonicalize()?, true, revision)
            }
        };
        let selection = SourceSelection {
            mode,
            directory,
            owned,
            revision,
        };
        sandbox.source = Some(selection.clone());
        catalog.save_sandbox(sandbox)?;
        Ok(selection)
    }
    /// False means the user's work was retained. Branches are never deleted here.
    pub fn remove_owned(
        &self,
        catalog: &Catalog,
        project: &Project,
        sandbox: &Sandbox,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let Some(source) = &sandbox.source else {
            return Ok(true);
        };
        if !source.owned {
            return Ok(false);
        }
        let expected = catalog.root.join("worktrees").join(sandbox.id.as_str());
        ensure!(
            !fs::symlink_metadata(&expected)?.file_type().is_symlink(),
            "Refusing to remove a redirected worktree"
        );
        ensure!(
            source.directory.canonicalize()? == expected.canonicalize()?,
            "Refusing to remove an unowned working directory"
        );
        let root = &project
            .repository
            .as_ref()
            .context("Repository is missing")?
            .directory;
        let trees = self.worktrees(root, cancel)?;
        let Some(tree) = trees.iter().find(|tree| {
            tree.directory.canonicalize().ok() == source.directory.canonicalize().ok()
        }) else {
            return Ok(false);
        };
        if tree.locked
            || !self
                .run(
                    &source.directory,
                    &strings(&[
                        "status",
                        "--porcelain",
                        "--ignored",
                        "--untracked-files=all",
                    ]),
                    cancel,
                )?
                .is_empty()
        {
            return Ok(false);
        }
        self.run(
            root,
            &strings(&[
                "worktree",
                "remove",
                &source.directory.display().to_string(),
            ]),
            cancel,
        )?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(root: &Path, args: &[&str]) -> Result<String> {
        Git::default().run(root, &strings(args), &AtomicBool::new(false))
    }
    #[test]
    fn local_repository_modes_preserve_checkout_and_dirty_owned_worktree() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-git-{}",
            sandbox_platform::docker::nonce()?
        ));
        fs::create_dir_all(root.join("repo with spaces"))?;
        let repo = root.join("repo with spaces");
        command(&repo, &["init", "-b", "main"])?;
        command(&repo, &["config", "user.email", "fixture@example.invalid"])?;
        command(&repo, &["config", "user.name", "Fixture"])?;
        command(&repo, &["config", "commit.gpgsign", "false"])?;
        command(&repo, &["config", "core.hooksPath", "/dev/null"])?;
        fs::write(repo.join("file"), "committed")?;
        command(&repo, &["add", "file"])?;
        command(&repo, &["commit", "-m", "fixture"])?;
        fs::write(repo.join("file"), "dirty main checkout")?;
        let catalog = Catalog::open(root.join("data/application.sqlite3"))?;
        let git = Git::default();
        let cancel = AtomicBool::new(false);
        let repository = git.repository(&repo, &cancel)?;
        let project =
            catalog.create_project("Fixture".into(), Some(repository), Default::default())?;
        let mut sandbox = catalog.create_sandbox(project.id.clone(), "Worktree".into())?;
        let managed = git.prepare(
            &catalog,
            &project,
            &mut sandbox,
            SourceMode::ManagedWorktree {
                branch: "feature/local".into(),
            },
            &cancel,
        )?;
        assert_eq!(
            fs::read_to_string(managed.directory.join("file"))?,
            "committed"
        );
        assert_eq!(
            fs::read_to_string(repo.join("file"))?,
            "dirty main checkout"
        );
        assert_eq!(git.worktrees(&repo, &cancel)?.len(), 2);
        let mut other = catalog.create_sandbox(project.id.clone(), "Existing".into())?;
        assert!(
            git.prepare(
                &catalog,
                &project,
                &mut other,
                SourceMode::ManagedWorktree {
                    branch: "feature/local".into()
                },
                &cancel
            )
            .is_err()
        );
        // A second record may not attach to a directory that already has its environment.
        assert!(
            git.prepare(
                &catalog,
                &project,
                &mut other,
                SourceMode::ExistingWorktree {
                    directory: managed.directory.clone()
                },
                &cancel
            )
            .is_err()
        );
        other.source = None;
        fs::write(managed.directory.join("file"), "linked commit")?;
        command(&managed.directory, &["add", "file"])?;
        command(&managed.directory, &["commit", "-m", "linked change"])?;
        let base = command(&managed.directory, &["rev-parse", "HEAD"])?;
        let mut derived = catalog.create_sandbox(project.id.clone(), "Derived".into())?;
        let derived_source = git.prepare_from(
            &catalog,
            &project,
            &mut derived,
            SourceMode::ManagedWorktree {
                branch: "feature/derived".into(),
            },
            Some(base.trim()),
            &cancel,
        )?;
        assert_eq!(
            fs::read_to_string(derived_source.directory.join("file"))?,
            "linked commit"
        );
        assert_eq!(
            command(&repo, &["branch", "--show-current"])?.trim(),
            "main"
        );
        assert!(git.remove_owned(&catalog, &project, &derived, &cancel)?);
        fs::write(managed.directory.join("untracked"), "preserve")?;
        assert!(!git.remove_owned(&catalog, &project, &sandbox, &cancel)?);
        fs::remove_file(managed.directory.join("untracked"))?;
        assert!(git.remove_owned(&catalog, &project, &sandbox, &cancel)?);
        let current = git.prepare(
            &catalog,
            &project,
            &mut other,
            SourceMode::CurrentCheckout,
            &cancel,
        )?;
        assert_eq!(current.directory, repo.canonicalize()?);
        assert!(
            git.branches(&repo, &cancel)?
                .contains(&"feature/local".to_owned())
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
