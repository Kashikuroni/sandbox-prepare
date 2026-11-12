//! Workspace selection. Menus never checkout branches or stop another sandbox.
use super::*;
use sandbox_core::{
    models::{Project, Sandbox, Scope, SourceMode},
    operations::Operation,
};
use sandbox_projects::git::{Git, Worktree};
use sandbox_ui::context_picker::{ContextAction, ContextItem, ContextPicker, PickerEvent};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};
#[derive(Clone)]
pub(super) struct RepositoryState {
    directory: PathBuf,
    trees: Vec<Worktree>,
    branches: Vec<String>,
    local: BTreeSet<String>,
}
pub(super) enum NavigationEvent {
    Git(ProjectId, anyhow::Result<RepositoryState>),
    Project(anyhow::Result<ProjectId>),
}
pub(super) struct ContextBar {
    projects: Vec<Project>,
    sandboxes: Vec<Sandbox>,
    snapshots: BTreeMap<ProjectId, RepositoryState>,
    last_query: BTreeMap<ProjectId, Instant>,
    jobs: BTreeMap<ProjectId, Arc<Operation>>,
    errors: BTreeMap<ProjectId, String>,
    pub last_sandbox: BTreeMap<ProjectId, Option<SandboxId>>,
    sender: mpsc::Sender<NavigationEvent>,
    receiver: mpsc::Receiver<NavigationEvent>,
}
impl Default for ContextBar {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            projects: vec![],
            sandboxes: vec![],
            snapshots: BTreeMap::new(),
            last_query: BTreeMap::new(),
            jobs: BTreeMap::new(),
            errors: BTreeMap::new(),
            last_sandbox: BTreeMap::new(),
            sender,
            receiver,
        }
    }
}
fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}
fn tree_name(tree: &Worktree, main: bool, records: &[Sandbox]) -> String {
    if main {
        return "main".into();
    }
    records
        .iter()
        .find(|s| {
            s.source
                .as_ref()
                .is_some_and(|source| source.owned && same_path(&source.directory, &tree.directory))
        })
        .map(|s| s.name.clone())
        .unwrap_or_else(|| {
            tree.directory
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
}
fn matching_sandbox<'a>(
    records: &'a [Sandbox],
    directory: &Path,
    current: Option<&SandboxId>,
) -> Option<&'a Sandbox> {
    records
        .iter()
        .filter(|s| {
            s.source
                .as_ref()
                .is_some_and(|source| same_path(&source.directory, directory))
        })
        .min_by_key(|s| if Some(&s.id) == current { 0 } else { 1 })
}
enum SourceTarget {
    Sandbox(SandboxId),
    Create(SourceMode),
}
fn worktree_target(
    records: &[Sandbox],
    trees: &[Worktree],
    directory: PathBuf,
    current: Option<&SandboxId>,
) -> SourceTarget {
    if let Some(sandbox) = matching_sandbox(records, &directory, current) {
        return SourceTarget::Sandbox(sandbox.id.clone());
    }
    let main = trees
        .first()
        .is_some_and(|tree| same_path(&tree.directory, &directory));
    SourceTarget::Create(if main {
        SourceMode::CurrentCheckout
    } else {
        SourceMode::ExistingWorktree { directory }
    })
}
fn branch_target(
    records: &[Sandbox],
    trees: &[Worktree],
    branch: &str,
    current: Option<&SandboxId>,
) -> Option<SourceTarget> {
    if branch == "@detached" {
        return None;
    }
    Some(
        if let Some(tree) = trees
            .iter()
            .find(|tree| tree.branch.as_deref() == Some(branch))
        {
            worktree_target(records, trees, tree.directory.clone(), current)
        } else {
            SourceTarget::Create(SourceMode::ManagedWorktree {
                branch: branch.into(),
            })
        },
    )
}
impl Workspace {
    pub(super) fn new_context_picker(
        label: &str,
        search: &str,
        icon: Option<Icon>,
        theme: ThemeMode,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<ContextPicker> {
        cx.new(|cx| ContextPicker::new(label, search, icon, theme, cx))
    }
    pub(super) fn bind_context_pickers(&self, cx: &mut Context<Self>) {
        cx.subscribe(
            &self.project_picker,
            |this, _, event: &PickerEvent, cx| match event {
                PickerEvent::Opened => {
                    this.close_context_pickers(0, cx);
                    this.refresh_choices(cx);
                }
                PickerEvent::Selected(value) => {
                    if let Ok(id) = ProjectId::try_from(value.clone()) {
                        this.choose_project(id, cx);
                    }
                }
                PickerEvent::Action("open-local") => this.open_local_project(cx),
                PickerEvent::Action("clone") => this.project_form(true, cx),
                _ => {}
            },
        )
        .detach();
        cx.subscribe(
            &self.sandbox_picker,
            |this, _, event: &PickerEvent, cx| match event {
                PickerEvent::Opened => {
                    this.close_context_pickers(3, cx);
                    this.refresh_choices(cx);
                }
                PickerEvent::Selected(value) => {
                    if let Ok(id) = SandboxId::try_from(value.clone()) {
                        this.select_context(this.project_id.clone(), Some(id), cx);
                    }
                }
                PickerEvent::Action("new-sandbox") => this.sandbox_form(None, cx),
                _ => {}
            },
        )
        .detach();
        cx.subscribe(
            &self.worktree_picker,
            |this, _, event: &PickerEvent, cx| match event {
                PickerEvent::Opened => {
                    this.close_context_pickers(1, cx);
                    this.query_git(true, cx);
                }
                PickerEvent::Selected(path) => this.choose_worktree(PathBuf::from(path), cx),
                PickerEvent::Action("new-worktree") => this.sandbox_form(
                    Some(SourceMode::ManagedWorktree {
                        branch: String::new(),
                    }),
                    cx,
                ),
                PickerEvent::Action("refresh-git") => this.query_git(true, cx),
                PickerEvent::Action("open-local") => this.open_local_project(cx),
                PickerEvent::Action("clone") => this.project_form(true, cx),
                _ => {}
            },
        )
        .detach();
        cx.subscribe(
            &self.branch_picker,
            |this, _, event: &PickerEvent, cx| match event {
                PickerEvent::Opened => {
                    this.close_context_pickers(2, cx);
                    this.query_git(true, cx);
                }
                PickerEvent::Selected(branch) => this.choose_branch(branch, cx),
                PickerEvent::Action("new-branch") => {
                    let branch = this.branch_picker.read(cx).query(cx);
                    this.sandbox_form(Some(SourceMode::ManagedWorktree { branch }), cx);
                }
                PickerEvent::Action("refresh-git") => this.query_git(true, cx),
                PickerEvent::Action("open-local") => this.open_local_project(cx),
                PickerEvent::Action("clone") => this.project_form(true, cx),
                _ => {}
            },
        )
        .detach();
    }
    fn close_context_pickers(&self, except: usize, cx: &mut Context<Self>) {
        for (index, picker) in [
            &self.project_picker,
            &self.worktree_picker,
            &self.branch_picker,
            &self.sandbox_picker,
        ]
        .into_iter()
        .enumerate()
        {
            if index != except {
                picker.update(cx, |p, cx| p.close(cx));
            }
        }
    }
    pub(super) fn refresh_choices(&mut self, cx: &mut Context<Self>) {
        if let Ok(records) = self.application.catalog.projects() {
            self.context_bar.projects = records;
        }
        if let Ok(records) = self.application.catalog.sandboxes() {
            self.context_bar.sandboxes = records;
        }
        let project = self
            .context_bar
            .projects
            .iter()
            .find(|p| p.id == self.project_id);
        let mut projects = vec![];
        if let Some(current) = project {
            projects.push(ContextItem::new(
                current.id.to_string(),
                current.name.clone(),
                "This Window",
            ));
        }
        projects.extend(
            self.context_bar
                .projects
                .iter()
                .filter(|p| p.id != self.project_id)
                .map(|p| ContextItem::new(p.id.to_string(), p.name.clone(), "Projects")),
        );
        let label = project
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Select project".into());
        self.project_picker.update(cx, |picker, cx| {
            picker.set(projects, self.project_id.to_string(), label, cx)
        });
        let sandboxes: Vec<_> = self
            .context_bar
            .sandboxes
            .iter()
            .filter(|s| s.project_id == self.project_id)
            .collect();
        let mut items = vec![];
        for record in &sandboxes {
            let state = self.application.sandbox_status(&record.id);
            let status = match state {
                Some(s) if s.busy => {
                    if s.stage.starts_with("Waiting") {
                        "Queued"
                    } else {
                        "Busy"
                    }
                }
                Some(s) if s.error.is_some() => "Error",
                Some(s) if s.running.iter().any(|v| *v) => "Running",
                Some(s) if s.active.is_some() => "Stopped",
                Some(_) => "Not created",
                None => "Not checked",
            };
            items.push(
                ContextItem::new(
                    record.id.to_string(),
                    record.name.clone(),
                    if Some(&record.id) == self.sandbox_id.as_ref() {
                        "This Window"
                    } else {
                        "Sandboxes"
                    },
                )
                .status(status),
            );
        }
        items.sort_by_key(|item| {
            if Some(item.value.as_str()) == self.sandbox_id.as_ref().map(SandboxId::as_str) {
                0
            } else {
                1
            }
        });
        let label = sandboxes
            .iter()
            .find(|s| Some(&s.id) == self.sandbox_id.as_ref())
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Select sandbox".into());
        self.sandbox_picker.update(cx, |picker, cx| {
            picker.set(
                items,
                self.sandbox_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                label,
                cx,
            )
        });
        self.refresh_source_choices(cx);
        self.query_git(false, cx);
    }
    fn refresh_source_choices(&self, cx: &mut Context<Self>) {
        let snapshot = self.context_bar.snapshots.get(&self.project_id);
        let current = self
            .context_bar
            .sandboxes
            .iter()
            .find(|s| Some(&s.id) == self.sandbox_id.as_ref());
        let directory = current
            .and_then(|s| s.source.as_ref().map(|s| &s.directory))
            .or_else(|| {
                self.context_bar
                    .projects
                    .iter()
                    .find(|p| p.id == self.project_id)
                    .and_then(|p| p.repository.as_ref().map(|r| &r.directory))
            });
        let has_repository = self
            .context_bar
            .projects
            .iter()
            .find(|p| p.id == self.project_id)
            .is_some_and(|p| p.repository.is_some());
        let mut worktree_value = directory
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut worktrees = vec![];
        let mut branches = vec![];
        let mut worktree_label = "No worktree".to_owned();
        let mut branch_label = "No branch".to_owned();
        let mut branch_value = String::new();
        if let Some(snapshot) = snapshot {
            for (index, tree) in snapshot.trees.iter().enumerate() {
                if tree.prunable {
                    continue;
                }
                let selected = directory.is_some_and(|path| same_path(path, &tree.directory));
                let name = tree_name(tree, index == 0, &self.context_bar.sandboxes);
                if selected {
                    worktree_label = name.clone();
                    worktree_value = tree.directory.display().to_string();
                    branch_label = tree.branch.clone().unwrap_or_else(|| {
                        format!(
                            "{} (detached)",
                            tree.revision.chars().take(8).collect::<String>()
                        )
                    });
                    branch_value = tree.branch.clone().unwrap_or_else(|| "@detached".into());
                }
                worktrees.push(
                    ContextItem::new(
                        tree.directory.display().to_string(),
                        name,
                        if selected {
                            "This Sandbox"
                        } else {
                            "Worktrees"
                        },
                    )
                    .detail(tree.directory.display().to_string())
                    .status(if tree.locked { "Locked" } else { "" }),
                );
            }
            if branch_value == "@detached" {
                branches.push(ContextItem::new(
                    "@detached",
                    branch_label.clone(),
                    "This Sandbox",
                ));
            }
            for branch in &snapshot.branches {
                branches.push(
                    ContextItem::new(
                        branch.clone(),
                        branch.clone(),
                        if *branch == branch_value {
                            "This Sandbox"
                        } else if snapshot.local.contains(branch) {
                            "Branches"
                        } else {
                            "Remote Branches"
                        },
                    )
                    .status(
                        if snapshot
                            .trees
                            .iter()
                            .any(|t| t.branch.as_ref() == Some(branch))
                        {
                            "Worktree"
                        } else {
                            ""
                        },
                    ),
                );
            }
            branches.sort_by_key(|item| match item.group.as_str() {
                "This Sandbox" => 0,
                "Branches" => 1,
                _ => 2,
            });
            worktrees.sort_by_key(|item| if item.group == "This Sandbox" { 0 } else { 1 });
        } else if directory.is_some() {
            worktree_label = "Loading…".into();
            branch_label = "Loading…".into();
        }
        let message = if let Some(error) = self.context_bar.errors.get(&self.project_id) {
            Some(error.clone())
        } else if self.context_bar.jobs.contains_key(&self.project_id) {
            Some("Reading Git…".into())
        } else if snapshot.is_none() {
            Some("Open a Git project to choose a source.".into())
        } else {
            None
        };
        self.worktree_picker.update(cx, |picker, cx| {
            picker.set(worktrees, worktree_value, worktree_label, cx);
            picker.message(message.clone(), cx);
        });
        self.branch_picker.update(cx, |picker, cx| {
            picker.set(branches, branch_value, branch_label, cx);
            picker.message(message, cx);
        });
        for (picker, kind, label) in [
            (&self.worktree_picker, "new-worktree", "New Worktree…"),
            (&self.branch_picker, "new-branch", "New Branch…"),
        ] {
            let actions = if has_repository {
                vec![
                    ContextAction { id: kind, label },
                    ContextAction {
                        id: "refresh-git",
                        label: "Refresh Git",
                    },
                ]
            } else {
                vec![
                    ContextAction {
                        id: "open-local",
                        label: "Open Local Folder…",
                    },
                    ContextAction {
                        id: "clone",
                        label: "Clone Repository…",
                    },
                ]
            };
            picker.update(cx, |picker, cx| picker.actions(actions, cx));
        }
    }
    fn query_git(&mut self, force: bool, cx: &mut Context<Self>) {
        let id = self.project_id.clone();
        if self.application.is_stopping() || self.context_bar.jobs.contains_key(&id) {
            return;
        }
        let Some(repository) = self
            .context_bar
            .projects
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.repository.clone())
        else {
            return;
        };
        if !force
            && self
                .context_bar
                .last_query
                .get(&id)
                .is_some_and(|last| last.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        self.context_bar
            .last_query
            .insert(id.clone(), Instant::now());
        let sender = self.context_bar.sender.clone();
        let project = id.clone();
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        let job = self.application.operations.spawn_blocking(
            Scope {
                project: Some(id.clone()),
                ..Default::default()
            },
            "Read titlebar Git context",
            flag,
            move |_| {
                let git = Git::default();
                let result = (|| {
                    Ok(RepositoryState {
                        trees: git.worktrees(&repository.directory, &cancel)?,
                        branches: git.branches(&repository.directory, &cancel)?,
                        local: git
                            .local_branches(&repository.directory, &cancel)?
                            .into_iter()
                            .collect(),
                        directory: repository.directory,
                    })
                })();
                let outcome = result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e: &anyhow::Error| anyhow::anyhow!("{e:#}"));
                let _ = sender.send(NavigationEvent::Git(project, result));
                outcome
            },
        );
        match job {
            Ok(job) => {
                self.context_bar.jobs.insert(id, job);
            }
            Err(error) => {
                self.context_bar.errors.insert(id, error.to_string());
            }
        }
        self.refresh_source_choices(cx);
    }
    pub(super) fn poll_context_events(&mut self, cx: &mut Context<Self>) {
        while let Ok(event) = self.context_bar.receiver.try_recv() {
            match event {
                NavigationEvent::Git(id, result) => {
                    self.context_bar.jobs.remove(&id);
                    match result {
                        Ok(snapshot) => {
                            let valid = self
                                .context_bar
                                .projects
                                .iter()
                                .find(|p| p.id == id)
                                .and_then(|p| p.repository.as_ref())
                                .is_some_and(|r| r.directory == snapshot.directory);
                            if valid {
                                self.context_bar.snapshots.insert(id.clone(), snapshot);
                                self.context_bar.errors.remove(&id);
                            }
                        }
                        Err(error) => {
                            self.context_bar
                                .errors
                                .insert(id.clone(), logging::redact(&format!("{error:#}")));
                        }
                    }
                    if id == self.project_id {
                        self.refresh_source_choices(cx);
                    }
                }
                NavigationEvent::Project(result) => match result {
                    Ok(id) => {
                        self.refresh_choices(cx);
                        self.choose_project(id.clone(), cx);
                        if self.project_id == id && self.sandbox_id.is_none() {
                            self.sandbox_form(None, cx);
                        }
                    }
                    Err(error) => {
                        self.context_error = Some(logging::redact(&format!("{error:#}")));
                        cx.notify();
                    }
                },
            }
        }
    }
    fn choose_project(&mut self, id: ProjectId, cx: &mut Context<Self>) {
        let sandbox = self
            .context_bar
            .last_sandbox
            .get(&id)
            .cloned()
            .flatten()
            .filter(|id| self.context_bar.sandboxes.iter().any(|s| &s.id == id))
            .or_else(|| {
                self.context_bar
                    .sandboxes
                    .iter()
                    .find(|s| s.project_id == id)
                    .map(|s| s.id.clone())
            });
        self.select_context(id, sandbox, cx);
    }
    fn apply_source_target(&mut self, target: SourceTarget, cx: &mut Context<Self>) {
        match target {
            SourceTarget::Sandbox(id) => self.select_context(self.project_id.clone(), Some(id), cx),
            SourceTarget::Create(mode) => self.sandbox_form(Some(mode), cx),
        }
    }
    fn choose_worktree(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        let records: Vec<_> = self
            .context_bar
            .sandboxes
            .iter()
            .filter(|s| s.project_id == self.project_id)
            .cloned()
            .collect();
        let trees = self
            .context_bar
            .snapshots
            .get(&self.project_id)
            .map(|s| s.trees.as_slice())
            .unwrap_or_default();
        let target = worktree_target(&records, trees, directory, self.sandbox_id.as_ref());
        self.apply_source_target(target, cx);
    }
    fn choose_branch(&mut self, branch: &str, cx: &mut Context<Self>) {
        let records: Vec<_> = self
            .context_bar
            .sandboxes
            .iter()
            .filter(|s| s.project_id == self.project_id)
            .cloned()
            .collect();
        let trees = self
            .context_bar
            .snapshots
            .get(&self.project_id)
            .map(|s| s.trees.as_slice())
            .unwrap_or_default();
        if let Some(target) = branch_target(&records, trees, branch, self.sandbox_id.as_ref()) {
            self.apply_source_target(target, cx);
        }
    }
    fn project_form(&mut self, clone: bool, cx: &mut Context<Self>) {
        self.active_section = Section::Projects;
        self.projects
            .update(cx, |view, cx| view.prepare_project_form(clone, cx));
        cx.notify();
    }
    fn sandbox_form(&mut self, mode: Option<SourceMode>, cx: &mut Context<Self>) {
        self.active_section = Section::Projects;
        self.projects
            .update(cx, |view, cx| view.prepare_sandbox_form(mode, cx));
        cx.notify();
    }
    fn open_local_project(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Git project".into()),
        });
        cx.spawn(async move |view, cx| match paths.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = view.update(cx, |view, cx| view.register_local_project(path, cx));
                }
            }
            Ok(Ok(None)) => {}
            error => {
                let _ = view.update(cx, |view, cx| {
                    view.context_error =
                        Some(format!("Could not open directory picker: {error:?}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }
    fn register_local_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let app = self.application.clone();
        let sender = self.context_bar.sender.clone();
        let storage = self
            .context_bar
            .projects
            .iter()
            .find(|p| p.id == self.project_id)
            .map(|p| p.storage.clone())
            .unwrap_or_default();
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        if let Err(error) = self.application.operations.spawn_blocking(
            Scope::default(),
            "Open local project",
            flag,
            move |_| {
                let result = (|| {
                    let repository = Git::default().repository(&path, &cancel)?;
                    if let Some(project) = app.catalog.projects()?.into_iter().find(|p| {
                        p.repository
                            .as_ref()
                            .is_some_and(|r| same_path(&r.directory, &repository.directory))
                    }) {
                        return Ok(project.id);
                    }
                    let name = repository
                        .directory
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    Ok(app
                        .add_project(
                            name,
                            repository.directory.display().to_string(),
                            false,
                            storage,
                            &cancel,
                        )?
                        .id)
                })();
                let outcome = result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e: &anyhow::Error| anyhow::anyhow!("{e:#}"));
                let _ = sender.send(NavigationEvent::Project(result));
                outcome
            },
        ) {
            self.context_error = Some(error.to_string());
            cx.notify();
        }
    }
    pub(super) fn configure_context_actions(&self, cx: &mut Context<Self>) {
        for (picker, actions) in [
            (
                &self.project_picker,
                vec![
                    ContextAction {
                        id: "open-local",
                        label: "Open Local Folder…",
                    },
                    ContextAction {
                        id: "clone",
                        label: "Clone Repository…",
                    },
                ],
            ),
            (
                &self.worktree_picker,
                vec![
                    ContextAction {
                        id: "new-worktree",
                        label: "New Worktree…",
                    },
                    ContextAction {
                        id: "refresh-git",
                        label: "Refresh Git",
                    },
                ],
            ),
            (
                &self.branch_picker,
                vec![
                    ContextAction {
                        id: "new-branch",
                        label: "New Branch…",
                    },
                    ContextAction {
                        id: "refresh-git",
                        label: "Refresh Git",
                    },
                ],
            ),
            (
                &self.sandbox_picker,
                vec![ContextAction {
                    id: "new-sandbox",
                    label: "New Sandbox…",
                }],
            ),
        ] {
            picker.update(cx, |picker, cx| picker.actions(actions, cx));
        }
    }
    pub(super) fn render_context_bar(&self, window: &Window) -> Div {
        let available = (f32::from(window.viewport_size().width)
            - if window.is_fullscreen() { 100. } else { 180. })
        .max(220.);
        div()
            .flex()
            .items_center()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .max_w(px((available * 0.30).min(210.)))
                    .child(self.project_picker.clone()),
            )
            .child(
                div()
                    .max_w(px((available * 0.19).min(135.)))
                    .child(self.worktree_picker.clone()),
            )
            .child(
                div()
                    .text_color(rgb(self
                        .settings
                        .theme
                        .palette(window.appearance())
                        .text_muted))
                    .child("/"),
            )
            .child(
                div()
                    .max_w(px((available * 0.23).min(170.)))
                    .child(self.branch_picker.clone()),
            )
            .child(
                div()
                    .ml_2()
                    .max_w(px((available * 0.28).min(190.)))
                    .child(self.sandbox_picker.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sandbox(id: &str, directory: &str) -> Sandbox {
        Sandbox {
            id: SandboxId::try_from(id.to_owned()).unwrap(),
            project_id: ProjectId::try_from("project".to_owned()).unwrap(),
            name: id.into(),
            root: "/data".into(),
            password: String::new(),
            root_password: String::new(),
            source: Some(sandbox_core::models::SourceSelection {
                mode: SourceMode::CurrentCheckout,
                directory: directory.into(),
                owned: false,
                revision: "revision".into(),
            }),
            compose: None,
        }
    }
    fn tree(branch: &str, directory: &str) -> Worktree {
        Worktree {
            directory: directory.into(),
            branch: Some(branch.into()),
            revision: "revision".into(),
            locked: false,
            prunable: false,
        }
    }
    #[test]
    fn source_selection_prefers_current_sandbox_and_never_checkout_a_busy_branch() {
        let records = vec![sandbox("first", "/repo"), sandbox("second", "/repo")];
        let trees = vec![tree("main", "/repo"), tree("feature", "/worktree")];
        assert!(
            matches!(branch_target(&records,&trees,"main",Some(&records[1].id)),Some(SourceTarget::Sandbox(id))if id==records[1].id)
        );
        assert!(
            matches!(branch_target(&records,&trees,"feature",None),Some(SourceTarget::Create(SourceMode::ExistingWorktree{directory}))if directory==Path::new("/worktree"))
        );
        assert!(matches!(
            branch_target(&[], &trees, "main", None),
            Some(SourceTarget::Create(SourceMode::CurrentCheckout))
        ));
        assert!(
            matches!(branch_target(&records,&trees,"new-feature",None),Some(SourceTarget::Create(SourceMode::ManagedWorktree{branch}))if branch=="new-feature")
        );
        assert!(branch_target(&records, &trees, "@detached", None).is_none());
    }
    #[test]
    fn nonexistent_different_directories_do_not_match() {
        assert!(!same_path(
            Path::new("/absent-one"),
            Path::new("/absent-two")
        ));
    }
}
