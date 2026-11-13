//! Read-only worktree review and explicit, bounded removal of app-owned linked worktrees.
use crate::git::Git;
use anyhow::{Context, Result, ensure};
use sandbox_core::models::{Project, Sandbox};
use sandbox_platform::catalog::Catalog;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug)]
pub struct LocalChange {
    pub status: String,
    pub path: String,
}
#[derive(Clone, Debug)]
pub struct RemovalReview {
    pub directory: Option<PathBuf>,
    pub owned: bool,
    pub can_delete: bool,
    pub locked: bool,
    pub warning: Option<String>,
    pub changes: Vec<LocalChange>,
    pub identity: [u8; 32],
    pub worktree: [u8; 32],
}
fn tree(path: &Path, cancel: &AtomicBool, hash: &mut Sha256) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "Removal review cancelled");
    let metadata = fs::symlink_metadata(path)?;
    hash.update(serde_json::to_vec(&path.to_string_lossy())?);
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
    } else if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_name() != ".git" {
                tree(&entry.path(), cancel, hash)?;
            }
        }
    }
    Ok(())
}
fn changes(status: &str) -> Vec<LocalChange> {
    let mut fields = status.split('\0').filter(|field| !field.is_empty());
    let mut result = vec![];
    while let Some(record) = fields.next() {
        let Some(code) = record.get(..2) else {
            continue;
        };
        let Some(path) = record.get(3..) else {
            continue;
        };
        let label = match code {
            "!!" => "Ignored",
            "??" => "Untracked",
            code if code.contains('U') || code == "AA" || code == "DD" => "Conflict",
            code if code.contains('R') => "Renamed",
            code if code.contains('D') => "Deleted",
            code if code.contains('A') => "Added",
            _ => "Modified",
        };
        let mut path = path.to_owned();
        if code.contains(['R', 'C']) {
            if let Some(from) = fields.next() {
                path = format!("{from} → {path}");
            }
        }
        result.push(LocalChange {
            status: label.into(),
            path,
        });
    }
    result
}
impl Git {
    pub fn review_removal(
        &self,
        catalog: &Catalog,
        project: &Project,
        sandbox: &Sandbox,
        cancel: &AtomicBool,
    ) -> Result<RemovalReview> {
        ensure!(!cancel.load(Ordering::Acquire), "Removal review cancelled");
        let identity: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&(sandbox, &project.repository))?).into();
        let mut review = RemovalReview {
            directory: sandbox
                .source
                .as_ref()
                .map(|source| source.directory.clone()),
            owned: sandbox.source.as_ref().is_some_and(|source| source.owned),
            can_delete: false,
            locked: false,
            warning: None,
            changes: vec![],
            identity,
            worktree: identity,
        };
        if !review.owned {
            review.warning = Some(
                if review.directory.is_some() {
                    "This working directory belongs to you and will be kept."
                } else {
                    "This sandbox has no working directory."
                }
                .into(),
            );
            return Ok(review);
        }
        let inspected = (|| -> Result<()> {
            let source = sandbox.source.as_ref().unwrap();
            let expected = catalog.root.join("worktrees").join(sandbox.id.as_str());
            let metadata =
                fs::symlink_metadata(&expected).context("The app-owned worktree is missing")?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "Worktree path is redirected or is not a directory; it can only be kept"
            );
            let path = source.directory.canonicalize()?;
            ensure!(
                path == expected.canonicalize()?,
                "Refusing to delete a directory outside this sandbox's owned worktree"
            );
            let root = &project
                .repository
                .as_ref()
                .context("Project repository is missing")?
                .directory;
            let trees = self.worktrees(root, cancel)?;
            ensure!(
                trees
                    .first()
                    .is_none_or(|main| main.directory.canonicalize().ok() != Some(path.clone())),
                "The main checkout cannot be deleted"
            );
            let registered = trees
                .iter()
                .find(|tree| tree.directory.canonicalize().ok() == Some(path.clone()))
                .context("Worktree is not registered in this project's Git repository")?;
            review.locked = registered.locked;
            let status = self.run(
                &path,
                &[
                    "status".into(),
                    "--porcelain=v1".into(),
                    "-z".into(),
                    "--ignored=matching".into(),
                    "--untracked-files=normal".into(),
                ],
                cancel,
            )?;
            review.changes = changes(&status);
            let mut hash = Sha256::new();
            hash.update(identity);
            hash.update(&status);
            hash.update(registered.revision.as_bytes());
            hash.update(registered.branch.as_deref().unwrap_or("").as_bytes());
            hash.update([registered.locked as u8]);
            tree(&path, cancel, &mut hash)?;
            review.worktree = hash.finalize().into();
            ensure!(
                !registered.locked,
                "Git has locked this worktree. Unlock it manually to enable worktree deletion, or keep it."
            );
            ensure!(
                !registered.prunable,
                "Worktree is unavailable; it can only be kept"
            );
            review.can_delete = true;
            Ok(())
        })();
        if let Err(error) = inspected {
            ensure!(!cancel.load(Ordering::Acquire), "Removal review cancelled");
            review.warning = Some(format!("{error:#}"));
        }
        Ok(review)
    }
    pub fn remove_reviewed_worktree(
        &self,
        catalog: &Catalog,
        project: &Project,
        sandbox: &Sandbox,
        expected: [u8; 32],
        cancel: &AtomicBool,
    ) -> Result<()> {
        let review = self.review_removal(catalog, project, sandbox, cancel)?;
        ensure!(
            review.can_delete,
            "{}",
            review
                .warning
                .as_deref()
                .unwrap_or("Worktree cannot be deleted")
        );
        ensure!(
            review.worktree == expected,
            "Worktree changed after confirmation. Review removal again; database data has not been deleted."
        );
        let root = &project
            .repository
            .as_ref()
            .context("Project repository is missing")?
            .directory;
        self.run(
            root,
            &[
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                review
                    .directory
                    .context("Worktree is missing")?
                    .display()
                    .to_string(),
            ],
            cancel,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox_core::models::SourceMode;
    struct Fixture {
        root: PathBuf,
        catalog: Catalog,
        project: Project,
        sandbox: Sandbox,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn command(path: &Path, args: &[&str]) -> Result<String> {
        Git::default().run(
            path,
            &args
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
            &AtomicBool::new(false),
        )
    }
    fn fixture() -> Result<Fixture> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-removal-{}",
            sandbox_platform::docker::nonce()?
        ));
        let repo = root.join("repo");
        fs::create_dir_all(&repo)?;
        command(&repo, &["init", "-b", "main"])?;
        command(&repo, &["config", "user.name", "Fixture"])?;
        command(&repo, &["config", "user.email", "fixture@example.invalid"])?;
        command(&repo, &["config", "commit.gpgsign", "false"])?;
        command(&repo, &["config", "core.hooksPath", "/dev/null"])?;
        fs::write(repo.join(".gitignore"), ".env\nignored/\n")?;
        fs::write(repo.join("file"), "committed")?;
        command(&repo, &["add", ".gitignore", "file"])?;
        command(&repo, &["commit", "-m", "fixture"])?;
        fs::write(repo.join(".env"), "local settings")?;
        let catalog = Catalog::open(root.join("data/application.sqlite3"))?;
        let git = Git::default();
        let cancel = AtomicBool::new(false);
        let project = catalog.create_project(
            "Fixture".into(),
            Some(git.repository(&repo, &cancel)?),
            Default::default(),
        )?;
        let mut sandbox = catalog.create_sandbox(project.id.clone(), "Linked fixture".into())?;
        git.prepare(
            &catalog,
            &project,
            &mut sandbox,
            SourceMode::ManagedWorktree {
                branch: "feature/removal".into(),
            },
            &cancel,
        )?;
        Ok(Fixture {
            root,
            catalog,
            project,
            sandbox,
        })
    }
    #[test]
    fn review_is_read_only_reports_ignored_changes_and_rejects_stale_or_locked_deletion()
    -> Result<()> {
        let fixture = fixture()?;
        let git = Git::default();
        let cancel = AtomicBool::new(false);
        let path = &fixture.sandbox.source.as_ref().unwrap().directory;
        fs::write(path.join("file"), "edited")?;
        fs::write(path.join("notes.txt"), "keep")?;
        let review = git.review_removal(
            &fixture.catalog,
            &fixture.project,
            &fixture.sandbox,
            &cancel,
        )?;
        assert!(review.owned && review.can_delete && !review.locked);
        for label in ["Modified", "Untracked", "Ignored"] {
            assert!(
                review.changes.iter().any(|change| change.status == label),
                "Missing {label} entry"
            );
        }
        assert_eq!(fs::read_to_string(path.join(".env"))?, "local settings");
        fs::write(path.join("file"), "another edit")?;
        assert!(
            git.remove_reviewed_worktree(
                &fixture.catalog,
                &fixture.project,
                &fixture.sandbox,
                review.worktree,
                &cancel
            )
            .unwrap_err()
            .to_string()
            .contains("changed after confirmation")
        );
        assert!(path.is_dir());
        let repo = &fixture.project.repository.as_ref().unwrap().directory;
        command(
            repo,
            &[
                "worktree",
                "lock",
                "--reason",
                "fixture lock",
                path.to_str().unwrap(),
            ],
        )?;
        let locked = git.review_removal(
            &fixture.catalog,
            &fixture.project,
            &fixture.sandbox,
            &cancel,
        )?;
        assert!(locked.locked && !locked.can_delete);
        assert!(
            git.remove_reviewed_worktree(
                &fixture.catalog,
                &fixture.project,
                &fixture.sandbox,
                locked.worktree,
                &cancel
            )
            .is_err()
        );
        assert!(path.is_dir());
        command(repo, &["worktree", "unlock", path.to_str().unwrap()])?;
        let fresh = git.review_removal(
            &fixture.catalog,
            &fixture.project,
            &fixture.sandbox,
            &cancel,
        )?;
        git.remove_reviewed_worktree(
            &fixture.catalog,
            &fixture.project,
            &fixture.sandbox,
            fresh.worktree,
            &cancel,
        )?;
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(repo.join("file"))?, "committed");
        command(
            repo,
            &["show-ref", "--verify", "refs/heads/feature/removal"],
        )?;
        Ok(())
    }
    #[test]
    fn user_checkout_and_redirected_owned_paths_can_only_be_kept() -> Result<()> {
        let mut fixture = fixture()?;
        let git = Git::default();
        let cancel = AtomicBool::new(false);
        let path = fixture.sandbox.source.as_ref().unwrap().directory.clone();
        fixture.sandbox.source.as_mut().unwrap().owned = false;
        let review = git.review_removal(
            &fixture.catalog,
            &fixture.project,
            &fixture.sandbox,
            &cancel,
        )?;
        assert!(!review.owned && !review.can_delete);
        assert!(
            git.remove_reviewed_worktree(
                &fixture.catalog,
                &fixture.project,
                &fixture.sandbox,
                review.worktree,
                &cancel
            )
            .is_err()
        );
        assert!(path.exists());
        #[cfg(unix)]
        {
            fixture.sandbox.source.as_mut().unwrap().owned = true;
            let saved = fixture.root.join("saved-worktree");
            fs::rename(&path, &saved)?;
            let external = fixture.root.join("user-directory");
            fs::create_dir_all(&external)?;
            fs::write(external.join("keep"), "user data")?;
            std::os::unix::fs::symlink(&external, &path)?;
            let redirected = git.review_removal(
                &fixture.catalog,
                &fixture.project,
                &fixture.sandbox,
                &cancel,
            )?;
            assert!(!redirected.can_delete);
            assert!(
                git.remove_reviewed_worktree(
                    &fixture.catalog,
                    &fixture.project,
                    &fixture.sandbox,
                    redirected.worktree,
                    &cancel
                )
                .is_err()
            );
            assert_eq!(fs::read_to_string(external.join("keep"))?, "user data");
        }
        Ok(())
    }
}
