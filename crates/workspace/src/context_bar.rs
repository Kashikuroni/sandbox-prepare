//! Workspace selection. Menus never checkout branches or stop another sandbox.
use super::source_menu::SourceEvent;
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
    Source(ProjectId, Option<SandboxId>, anyhow::Result<SandboxId>),
}
pub(super) struct ContextBar {
    projects: Vec<Project>,
    sandboxes: Vec<Sandbox>,
    snapshots: BTreeMap<ProjectId, RepositoryState>,
    last_query: BTreeMap<ProjectId, Instant>,
    jobs: BTreeMap<ProjectId, Arc<Operation>>,
    errors: BTreeMap<ProjectId, String>,
    source_jobs: BTreeMap<ProjectId, Arc<Operation>>,
    source_errors: BTreeMap<ProjectId, String>,
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
            source_jobs: BTreeMap::new(),
            source_errors: BTreeMap::new(),
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
                    this.source_menu.update(cx, |menu, cx| menu.close(cx));
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
            &self.source_menu,
            |this, _, event: &SourceEvent, cx| match event {
                SourceEvent::Opened => {
                    this.project_picker.update(cx, |p, cx| p.close(cx));
                    this.query_git(true, cx);
                }
                SourceEvent::Open(mode) => this.open_working_copy(mode.clone(), false, cx),
                SourceEvent::NewBranch(branch) => this.open_working_copy(
                    SourceMode::ManagedWorktree {
                        branch: branch.clone(),
                    },
                    true,
                    cx,
                ),
                SourceEvent::Legacy(id) => {
                    this.select_context(this.project_id.clone(), Some(id.clone()), cx)
                }
                SourceEvent::Cancel => {
                    if let Some(job) = this.context_bar.source_jobs.get(&this.project_id) {
                        job.cancel();
                    }
                }
                SourceEvent::Configure => {
                    if this.sandbox_id.is_some() {
                        this.active_section = Section::Projects;
                        this.projects.update(cx, |view, cx| view.show_launch(cx));
                    } else {
                        this.context_error = Some("Choose a working copy first.".into());
                        cx.notify();
                    }
                }
            },
        )
        .detach();
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
        if let Ok(recent) = self.application.catalog.recent_project_ids() {
            projects.extend(
                recent
                    .iter()
                    .filter(|id| **id != self.project_id)
                    .filter_map(|id| self.context_bar.projects.iter().find(|p| &p.id == id))
                    .map(|p| ContextItem::new(p.id.to_string(), p.name.clone(), "Recent Projects")),
            );
        }
        let label = project
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Select project".into());
        self.project_picker.update(cx, |picker, cx| {
            picker.set(projects, self.project_id.to_string(), label, cx)
        });
        self.refresh_source_choices(cx);
        self.query_git(false, cx);
    }
    fn refresh_source_choices(&self, cx: &mut Context<Self>) {
        let snapshot = self.context_bar.snapshots.get(&self.project_id);
        let records: Vec<_> = self
            .context_bar
            .sandboxes
            .iter()
            .filter(|s| s.project_id == self.project_id)
            .collect();
        let current = records
            .iter()
            .find(|s| Some(&s.id) == self.sandbox_id.as_ref());
        let source = current.and_then(|s| s.source.as_ref());
        let selected = source.and_then(|s| {
            snapshot.and_then(|snapshot| {
                snapshot
                    .trees
                    .iter()
                    .find(|t| same_path(&t.directory, &s.directory))
            })
        });
        let label = selected
            .map(|tree| {
                tree.branch
                    .clone()
                    .unwrap_or_else(|| tree.revision.chars().take(8).collect())
            })
            .map(|branch| {
                format!(
                    "{} / {branch}",
                    if source.is_some_and(|s| s.owned) {
                        "Isolated worktree"
                    } else if source.is_some_and(|s| matches!(s.mode, SourceMode::CurrentCheckout))
                    {
                        "Local checkout"
                    } else {
                        "Existing worktree"
                    }
                )
            })
            .unwrap_or_else(|| {
                if source.is_some() {
                    "Working copy unavailable".into()
                } else if current.is_some() {
                    "Legacy environment".into()
                } else {
                    "Choose working copy".into()
                }
            });
        let mut branches = vec![];
        let mut worktrees = vec![];
        if let Some(snapshot) = snapshot {
            for branch in &snapshot.branches {
                let local = if snapshot.local.contains(branch) {
                    branch.as_str()
                } else {
                    branch.split_once('/').map(|(_, b)| b).unwrap_or(branch)
                };
                if !snapshot
                    .trees
                    .iter()
                    .any(|t| t.branch.as_deref() == Some(local))
                {
                    branches.push(ContextItem::new(
                        branch.clone(),
                        branch.clone(),
                        "Free branches",
                    ));
                }
            }
            for tree in snapshot.trees.iter().skip(1).filter(|t| !t.prunable) {
                let label = tree
                    .branch
                    .clone()
                    .unwrap_or_else(|| tree.revision.chars().take(8).collect());
                worktrees.push(
                    ContextItem::new(tree.directory.display().to_string(), label, "Worktrees")
                        .detail(tree.directory.display().to_string()),
                );
            }
        }
        let status = current
            .and_then(|s| self.application.sandbox_status(&s.id))
            .map(|state| {
                if state.busy {
                    state.stage
                } else if state.error.is_some() {
                    "Environment needs attention".into()
                } else if state.running.iter().any(|r| *r) {
                    "Running".into()
                } else if state.active.is_some() {
                    "Stopped".into()
                } else {
                    "Databases not created".into()
                }
            })
            .unwrap_or_else(|| "Choose a working copy, then configure launch".into());
        let message = self
            .context_bar
            .source_errors
            .get(&self.project_id)
            .or_else(|| self.context_bar.errors.get(&self.project_id))
            .cloned()
            .or_else(|| {
                if self.context_bar.source_jobs.contains_key(&self.project_id) {
                    Some("Preparing working copy…".into())
                } else if snapshot.is_none() {
                    Some(
                        if self.context_bar.jobs.contains_key(&self.project_id) {
                            "Reading Git…"
                        } else {
                            "Open a Git project to choose a working copy."
                        }
                        .into(),
                    )
                } else {
                    None
                }
            });
        // Legacy records remain reachable until the user explicitly removes or resolves them.
        let legacy = records
            .iter()
            .filter(|s| {
                s.source.as_ref().is_none_or(|source| {
                    records
                        .iter()
                        .filter(|other| {
                            other
                                .source
                                .as_ref()
                                .is_some_and(|v| same_path(&v.directory, &source.directory))
                        })
                        .count()
                        > 1
                        || snapshot.is_none_or(|snapshot| {
                            !snapshot
                                .trees
                                .iter()
                                .any(|t| same_path(&t.directory, &source.directory))
                        })
                })
            })
            .map(|s| (s.id.clone(), s.name.clone()))
            .collect();
        self.source_menu.update(cx, |menu, cx| {
            menu.set(
                label,
                source.map(|s| s.mode.clone()),
                source
                    .map(|s| s.directory.display().to_string())
                    .unwrap_or_default(),
                status,
                snapshot.is_some(),
                self.context_bar.source_jobs.contains_key(&self.project_id),
                message,
                branches,
                worktrees,
                legacy,
                cx,
            )
        });
    }
    fn open_working_copy(&mut self, mode: SourceMode, new_branch: bool, cx: &mut Context<Self>) {
        if self.context_bar.source_jobs.contains_key(&self.project_id) {
            return;
        }
        if new_branch {
            let SourceMode::ManagedWorktree { branch } = &mode else {
                return;
            };
            if branch.trim().is_empty() {
                self.context_bar.source_errors.insert(
                    self.project_id.clone(),
                    "Enter a name for the new branch.".into(),
                );
                self.context_error = self
                    .context_bar
                    .source_errors
                    .get(&self.project_id)
                    .cloned();
                self.refresh_source_choices(cx);
                return;
            }
            if self
                .context_bar
                .snapshots
                .get(&self.project_id)
                .is_some_and(|s| s.branches.iter().any(|b| b == branch))
            {
                self.context_bar.source_errors.insert(
                    self.project_id.clone(),
                    "Branch already exists. Select a free branch from the list.".into(),
                );
                self.context_error = self
                    .context_bar
                    .source_errors
                    .get(&self.project_id)
                    .cloned();
                self.refresh_source_choices(cx);
                return;
            }
        }
        let base = self
            .context_bar
            .sandboxes
            .iter()
            .find(|s| Some(&s.id) == self.sandbox_id.as_ref())
            .and_then(|s| s.source.as_ref())
            .map(|source| source.directory.clone())
            .or_else(|| {
                self.context_bar
                    .projects
                    .iter()
                    .find(|p| p.id == self.project_id)
                    .and_then(|p| p.repository.as_ref())
                    .map(|r| r.directory.clone())
            });
        self.context_error = None;
        self.context_bar.source_errors.remove(&self.project_id);
        let app = self.application.clone();
        let project = self.project_id.clone();
        let previous = self.sandbox_id.clone();
        let sender = self.context_bar.sender.clone();
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        let id = project.clone();
        match self.application.operations.spawn_blocking(
            Scope {
                project: Some(project.clone()),
                ..Default::default()
            },
            "Open sandbox working copy",
            flag,
            move |_| {
                let result = (|| {
                    let revision =
                        if new_branch {
                            Some(Git::default().head_revision(
                                base.as_deref().ok_or_else(|| {
                                    anyhow::anyhow!("Choose a working copy first")
                                })?,
                                &cancel,
                            )?)
                        } else {
                            None
                        };
                    app.open_working_copy(&project, mode, revision.as_deref(), &cancel)
                        .map(|s| s.id)
                })();
                let outcome = result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("{e:#}"));
                let _ = sender.send(NavigationEvent::Source(project, previous, result));
                outcome
            },
        ) {
            Ok(job) => {
                self.context_bar.source_jobs.insert(id, job);
            }
            Err(error) => {
                self.context_bar.source_errors.insert(id, error.to_string());
            }
        }
        self.refresh_source_choices(cx);
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
                NavigationEvent::Source(project, previous, result) => {
                    self.context_bar.source_jobs.remove(&project);
                    match result {
                        Ok(id) => {
                            self.context_bar.source_errors.remove(&project);
                            self.context_bar
                                .last_sandbox
                                .insert(project.clone(), Some(id.clone()));
                            if self.project_id == project && self.sandbox_id == previous {
                                self.select_context(project, Some(id), cx);
                                self.active_section = Section::Projects;
                                self.projects.update(cx, |view, cx| view.show_launch(cx));
                            }
                        }
                        Err(error) => {
                            let message = logging::redact(&format!("{error:#}"));
                            if self.project_id == project {
                                self.context_error = Some(message.clone());
                            }
                            self.context_bar.source_errors.insert(project, message);
                        }
                    }
                    self.refresh_choices(cx);
                }
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
    fn project_form(&mut self, clone: bool, cx: &mut Context<Self>) {
        self.active_section = Section::Projects;
        self.projects
            .update(cx, |view, cx| view.prepare_project_form(clone, cx));
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
        self.project_picker.update(cx, |p, cx| {
            p.actions(
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
                cx,
            )
        });
    }
    pub(super) fn render_context_bar(&self, window: &Window) -> Div {
        let available = (f32::from(window.viewport_size().width)
            - if window.is_fullscreen() { 40. } else { 120. })
        .max(200.);
        div()
            .flex()
            .items_center()
            .min_w_0()
            .gap(px(2.))
            .child(
                div()
                    .max_w(px((available * 0.4).min(210.)))
                    .child(self.project_picker.clone()),
            )
            .child(
                div()
                    .max_w(px((available * 0.6).min(360.)))
                    .child(self.source_menu.clone()),
            )
    }
}
