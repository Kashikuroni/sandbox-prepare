use crate::{
    text_input::TextInput,
    theme::{Palette, ThemeMode, ThemePalette},
};
use gpui::{prelude::*, *};
use sandbox_core::{models::*, operations::Operation};
use sandbox_projects::compose;
use sandbox_runtime::Runtime;
use sandbox_ui::select::{OptionItem, Select};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool, mpsc},
};

pub struct Changed {
    pub project: ProjectId,
    pub sandbox: Option<SandboxId>,
}
enum Event {
    Created(ProjectId, Option<SandboxId>),
    Discovered(Vec<compose::Candidate>),
    Inspected(
        serde_json::Value,
        std::collections::BTreeMap<String, MountRole>,
    ),
    Saved(String),
    Configured(ComposeLaunchSpec),
}
#[derive(Clone)]
struct Mount {
    key: String,
    role: Entity<Select>,
    path: Entity<TextInput>,
}
#[derive(Clone)]
struct LaunchDraft {
    spec: ComposeLaunchSpec,
    candidates: Vec<compose::Candidate>,
    profiles: Vec<String>,
    services: Vec<String>,
    mounts: Vec<Mount>,
    inputs: [Entity<TextInput>; 10],
    selects: [Entity<Select>; 4],
}
pub struct ProjectsView {
    app: Arc<Runtime>,
    project: ProjectId,
    sandbox: Option<SandboxId>,
    theme: ThemeMode,
    project_name: Entity<TextInput>,
    location: Entity<TextInput>,
    clone_remote: bool,
    candidates: Vec<compose::Candidate>,
    spec: ComposeLaunchSpec,
    profiles: Vec<String>,
    services: Vec<String>,
    mounts: Vec<Mount>,
    mysql_database: Entity<TextInput>,
    ch_database: Entity<TextInput>,
    probe_service: Entity<TextInput>,
    probe_port: Entity<TextInput>,
    probe_path: Entity<TextInput>,
    advanced: bool,
    mysql: Entity<TextInput>,
    clickhouse: Entity<TextInput>,
    env_path: Entity<TextInput>,
    env_service: Entity<Select>,
    binding_service: Entity<Select>,
    binding_db: Entity<Select>,
    binding_parameter: Entity<Select>,
    variable: Entity<TextInput>,
    database_name: Entity<TextInput>,
    mysql_prefix: Entity<TextInput>,
    ch_prefix: Entity<TextInput>,
    operation: Option<Arc<Operation>>,
    events: mpsc::Sender<(u64, anyhow::Result<Event>)>,
    request: u64,
    error: Option<String>,
    notice: Option<String>,
    tab: usize,
    remove_confirmation: bool,
    drafts: std::collections::BTreeMap<SandboxId, LaunchDraft>,
    form_focus: Option<()>,
    binding_error: Option<String>,
    save_error: Option<String>,
}
impl EventEmitter<Changed> for ProjectsView {}
impl ProjectsView {
    pub fn new(
        app: Arc<Runtime>,
        project: ProjectId,
        sandbox: Option<SandboxId>,
        theme: ThemeMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let (events, rx) = mpsc::channel();
        cx.spawn(async move |view, cx| {
            loop {
                Timer::after(std::time::Duration::from_millis(100)).await;
                if view
                    .update(cx, |view, cx| {
                        while let Ok((request, event)) = rx.try_recv() {
                            if request != view.request {
                                continue;
                            }
                            let action = view.operation.take().map(|job| job.name.clone());
                            match event {
                                Ok(event) => view.receive(event, cx),
                                Err(e) => {
                                    let message =
                                        Some(sandbox_telemetry::logging::redact(&format!("{e:#}")));
                                    if action.as_deref() == Some("Save project launch profile") {
                                        view.save_error = message;
                                    } else {
                                        view.error = message;
                                    }
                                }
                            };
                            cx.notify();
                        }
                        if let Some(job) = &view.operation
                            && let sandbox_core::operations::Status::Failed(error) = job.status()
                        {
                            view.error = Some(sandbox_telemetry::logging::redact(&error));
                            view.operation = None;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let input = |value: &str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(value.into(), false, theme, cx))
        };
        let select =
            |label: &str, options: &[(&str, &str)], value: &str, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut s = Select::new(label, theme, cx);
                    s.set(
                        options
                            .iter()
                            .map(|(v, l)| OptionItem::new(*v, *l))
                            .collect(),
                        value.into(),
                        cx,
                    );
                    s
                })
            };
        let mut view = Self {
            app,
            project,
            sandbox,
            theme,
            project_name: input("", cx),
            location: input("", cx),
            clone_remote: false,
            candidates: vec![],
            spec: Default::default(),
            profiles: vec![],
            services: vec![],
            mounts: vec![],
            mysql_database: input("", cx),
            ch_database: input("", cx),
            probe_service: input("", cx),
            probe_port: input("", cx),
            probe_path: input("", cx),
            advanced: false,
            mysql: input("mysql", cx),
            clickhouse: input("clickhouse", cx),
            env_path: input("", cx),
            env_service: select("Import for", &[("*", "All services")], "*", cx),
            binding_service: select("Service", &[], "", cx),
            binding_db: select(
                "Database",
                &[("mysql", "MySQL"), ("clickhouse", "ClickHouse")],
                "mysql",
                cx,
            ),
            binding_parameter: select(
                "Parameter",
                &[
                    ("host", "Host"),
                    ("port", "Port"),
                    ("user", "User"),
                    ("password", "Password"),
                    ("database", "Database"),
                    ("url", "Connection URL"),
                ],
                "host",
                cx,
            ),
            variable: input("", cx),
            database_name: input("", cx),
            mysql_prefix: input("mysql/", cx),
            ch_prefix: input("ch/", cx),
            operation: None,
            events,
            request: 0,
            error: None,
            notice: None,
            tab: 0,
            remove_confirmation: false,
            drafts: Default::default(),
            form_focus: None,
            binding_error: None,
            save_error: None,
        };
        view.load(cx);
        view
    }
    pub fn prepare_project_form(&mut self, clone_remote: bool, cx: &mut Context<Self>) {
        self.tab = 0;
        self.error = None;
        self.notice = None;
        if self.clone_remote != clone_remote {
            self.location
                .update(cx, |input, cx| input.set_value(String::new(), cx));
        }
        self.clone_remote = clone_remote;
        self.form_focus = Some(());
        cx.notify();
    }
    pub fn show_launch(&mut self, cx: &mut Context<Self>) {
        self.tab = 2;
        cx.notify();
    }

    pub fn context(
        &mut self,
        project: ProjectId,
        sandbox: Option<SandboxId>,
        cx: &mut Context<Self>,
    ) {
        if self.project == project && self.sandbox == sandbox {
            return;
        }
        self.save_draft();
        self.operation = None;
        self.request += 1;
        self.project = project;
        self.sandbox = sandbox;
        self.load(cx);
        cx.notify();
    }
    fn fill_database_names(&self, cx: &mut Context<Self>) {
        if let Ok(env) = self.app.catalog.environment(&self.project, "*") {
            for (key, field) in [
                ("DB_NAME", &self.mysql_database),
                ("CLICKHOUSE_DATABASE", &self.ch_database),
            ] {
                if field.read(cx).value().trim().is_empty() {
                    if let Some(value) = env.get(key) {
                        field.update(cx, |field, cx| field.set_value(value.clone(), cx));
                    }
                }
            }
        }
    }
    fn save_draft(&mut self) {
        if let Some(id) = &self.sandbox {
            if self.app.catalog.sandbox(id).is_err() {
                self.drafts.remove(id);
                return;
            }
            self.drafts.insert(
                id.clone(),
                LaunchDraft {
                    spec: self.spec.clone(),
                    candidates: self.candidates.clone(),
                    profiles: self.profiles.clone(),
                    services: self.services.clone(),
                    mounts: self.mounts.clone(),
                    inputs: [
                        self.mysql.clone(),
                        self.clickhouse.clone(),
                        self.env_path.clone(),
                        self.variable.clone(),
                        self.database_name.clone(),
                        self.mysql_database.clone(),
                        self.ch_database.clone(),
                        self.probe_service.clone(),
                        self.probe_port.clone(),
                        self.probe_path.clone(),
                    ],
                    selects: [
                        self.env_service.clone(),
                        self.binding_service.clone(),
                        self.binding_db.clone(),
                        self.binding_parameter.clone(),
                    ],
                },
            );
        }
    }
    fn launch_select(
        &self,
        label: &str,
        options: &[(&str, &str)],
        value: &str,
        cx: &mut Context<Self>,
    ) -> Entity<Select> {
        cx.new(|cx| {
            let mut picker = Select::new(label, self.theme, cx);
            picker.set(
                options
                    .iter()
                    .map(|(value, label)| OptionItem::new(*value, *label))
                    .collect(),
                value.into(),
                cx,
            );
            picker
        })
    }
    fn load(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.notice = None;
        self.remove_confirmation = false;
        self.binding_error = None;
        self.save_error = None;
        // New entities prevent another sandbox's retained draft being mutated through shared handles.
        let input = |value: &str, cx: &mut Context<Self>| {
            cx.new(|cx| TextInput::new(value.into(), false, self.theme, cx))
        };
        self.mysql_database = input("", cx);
        self.ch_database = input("", cx);
        self.probe_service = input("", cx);
        self.probe_port = input("", cx);
        self.probe_path = input("", cx);
        self.mysql = input("mysql", cx);
        self.clickhouse = input("clickhouse", cx);
        self.env_path = input("", cx);
        self.variable = input("", cx);
        self.database_name = input("", cx);
        self.env_service = self.launch_select("Import for", &[("*", "All services")], "*", cx);
        self.binding_service = self.launch_select("Service", &[], "", cx);
        self.binding_db = self.launch_select(
            "Database",
            &[("mysql", "MySQL"), ("clickhouse", "ClickHouse")],
            "mysql",
            cx,
        );
        self.binding_parameter = self.launch_select(
            "Parameter",
            &[
                ("host", "Host"),
                ("port", "Port"),
                ("user", "User"),
                ("password", "Password"),
                ("database", "Database"),
                ("url", "Connection URL"),
            ],
            "host",
            cx,
        );
        self.candidates.clear();
        self.profiles.clear();
        self.services.clear();
        self.mounts.clear();
        if let Ok(project) = self.app.catalog.project(&self.project) {
            for (id, field) in [
                ("mysql", &self.mysql_prefix),
                ("clickhouse", &self.ch_prefix),
            ] {
                if let Some(db) = project
                    .databases
                    .iter()
                    .find(|(key, _)| key.as_str() == id)
                    .map(|(_, v)| v)
                {
                    field.update(cx, |input, cx| input.set_value(db.prefix.clone(), cx));
                }
            }
        }
        self.spec = self
            .sandbox
            .as_ref()
            .and_then(|id| self.app.catalog.sandbox(id).ok())
            .and_then(|s| s.compose)
            .unwrap_or_default();
        if self.spec.files.is_empty() {
            self.spec = self
                .app
                .catalog
                .launch_profile(&self.project)
                .ok()
                .flatten()
                .unwrap_or_default();
        }
        if let Some(names) = &self.spec.automatic_connections {
            self.mysql_database
                .update(cx, |field, cx| field.set_value(names.mysql.clone(), cx));
            self.ch_database.update(cx, |field, cx| {
                field.set_value(names.clickhouse.clone(), cx)
            });
        }
        if let Some(probe) = &self.spec.readiness_probe {
            self.probe_service
                .update(cx, |field, cx| field.set_value(probe.service.clone(), cx));
            self.probe_port
                .update(cx, |field, cx| field.set_value(probe.port.to_string(), cx));
            self.probe_path
                .update(cx, |field, cx| field.set_value(probe.path.clone(), cx));
        }
        self.fill_database_names(cx);
        for (role, field) in [("mysql", &self.mysql), ("clickhouse", &self.clickhouse)] {
            let name = self
                .spec
                .databases
                .iter()
                .find(|(_, id)| id.as_str() == role)
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| role.into());
            field.update(cx, |input, cx| input.set_value(name, cx));
        }
        if let Some(draft) = self
            .sandbox
            .as_ref()
            .and_then(|id| self.drafts.get(id))
            .cloned()
        {
            self.spec = draft.spec;
            self.candidates = draft.candidates;
            self.profiles = draft.profiles;
            self.services = draft.services;
            self.mounts = draft.mounts;
            [
                self.mysql,
                self.clickhouse,
                self.env_path,
                self.variable,
                self.database_name,
                self.mysql_database,
                self.ch_database,
                self.probe_service,
                self.probe_port,
                self.probe_path,
            ] = draft.inputs;
            [
                self.env_service,
                self.binding_service,
                self.binding_db,
                self.binding_parameter,
            ] = draft.selects;
            self.set_theme(self.theme, cx);
        } else if !self.spec.files.is_empty() {
            self.inspect(cx);
        } else if self
            .sandbox
            .as_ref()
            .and_then(|id| self.app.catalog.sandbox(id).ok())
            .is_some_and(|s| s.source.is_some())
        {
            self.discover(cx);
        }
    }
    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        for field in [
            &self.project_name,
            &self.location,
            &self.mysql,
            &self.mysql_database,
            &self.ch_database,
            &self.probe_service,
            &self.probe_port,
            &self.probe_path,
            &self.clickhouse,
            &self.env_path,
            &self.variable,
            &self.database_name,
            &self.mysql_prefix,
            &self.ch_prefix,
        ] {
            field.update(cx, |input, cx| input.set_theme(theme, cx));
        }
        for field in [
            &self.env_service,
            &self.binding_service,
            &self.binding_db,
            &self.binding_parameter,
        ] {
            field.update(cx, |s, cx| s.set_theme(theme, cx));
        }
        for mount in &self.mounts {
            mount.role.update(cx, |s, cx| s.set_theme(theme, cx));
            mount
                .path
                .update(cx, |input, cx| input.set_theme(theme, cx));
        }
        cx.notify();
    }
    fn run(
        &mut self,
        name: &str,
        work: impl FnOnce(Arc<Runtime>, Arc<AtomicBool>) -> anyhow::Result<Event> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.operation.is_some() {
            return;
        }
        self.error = None;
        self.notice = None;
        self.save_error = None;
        self.request += 1;
        let request = self.request;
        let app = self.app.clone();
        let events = self.events.clone();
        let flag = Arc::new(AtomicBool::new(false));
        let signal = flag.clone();
        let result = self.app.operations.spawn_blocking(
            sandbox_core::models::Scope {
                project: Some(self.project.clone()),
                sandbox: self.sandbox.clone(),
                operation: None,
            },
            name,
            flag,
            move |_| {
                let result = work(app, signal);
                let outcome = result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("{e:#}"));
                let _ = events.send((request, result));
                outcome
            },
        );
        match result {
            Ok(job) => self.operation = Some(job),
            Err(e) => self.error = Some(e.to_string()),
        }
        cx.notify();
    }
    fn receive(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Created(project, sandbox) => {
                // Workspace owns selection and may refuse a project switch with unsaved Settings.
                // Its context() call saves the outgoing draft before loading the new sandbox.
                let created_sandbox = sandbox.is_some();
                self.tab = if created_sandbox { 2 } else { 0 };
                cx.emit(Changed { project, sandbox });
                self.notice = Some(
                    if created_sandbox {
                        "Sandbox created. Configure Launch, then create the databases in Sandboxes."
                    } else {
                        "Project opened. Choose a working copy from the titlebar."
                    }
                    .into(),
                );
            }
            Event::Discovered(candidates) => {
                self.candidates = candidates;
                self.notice = Some(
                    "Select files in merge order, then inspect the combined configuration.".into(),
                );
            }
            Event::Inspected(document, inferred_mounts) => {
                self.services = compose::service_names(&document);
                self.profiles = compose::profile_names(&document);
                let options = || {
                    self.services
                        .iter()
                        .map(|v| OptionItem::new(v.clone(), v.clone()))
                        .collect()
                };
                self.binding_service
                    .update(cx, |s, cx| s.set(options(), String::new(), cx));
                let mut all = options();
                all.insert(0, OptionItem::new("*", "All services"));
                self.env_service
                    .update(cx, |s, cx| s.set(all, "*".into(), cx));
                self.mounts.clear();
                let source = self
                    .sandbox
                    .as_ref()
                    .and_then(|id| self.app.catalog.sandbox(id).ok())
                    .and_then(|s| s.source)
                    .map(|s| s.directory);
                if let Some(services) = document["services"].as_object() {
                    for (name, service) in services {
                        if let Some(mounts) = service["volumes"].as_array() {
                            for mount in mounts.iter().filter(|m| m["type"] == "bind") {
                                let target = mount["target"].as_str().unwrap_or("");
                                let key = format!("{name}:{target}");
                                let path = mount["source"].as_str().unwrap_or("");
                                let default = if mount["read_only"] == true {
                                    "config"
                                } else if source.as_ref().is_some_and(|p| p == &PathBuf::from(path))
                                {
                                    "source"
                                } else {
                                    "data"
                                };
                                let value = match self.spec.mounts.get(&key) {
                                    Some(MountRole::Source) => "source",
                                    Some(MountRole::Configuration) => "config",
                                    Some(MountRole::Data) => "data",
                                    None => match inferred_mounts.get(&key) {
                                        Some(MountRole::Source) => "source",
                                        Some(MountRole::Configuration) => "config",
                                        Some(MountRole::Data) => "data",
                                        None => default,
                                    },
                                };
                                let role = cx.new(|cx| {
                                    let mut s = Select::new("Mount role", self.theme, cx);
                                    s.set(
                                        [
                                            ("source", "Source code"),
                                            ("config", "Read-only configuration"),
                                            ("data", "Private sandbox data"),
                                        ]
                                        .into_iter()
                                        .map(|(v, l)| OptionItem::new(v, l))
                                        .collect(),
                                        value.into(),
                                        cx,
                                    );
                                    s
                                });
                                let path = self
                                    .spec
                                    .mount_paths
                                    .get(&key)
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| path.into());
                                let input =
                                    cx.new(|cx| TextInput::new(path, false, self.theme, cx));
                                self.mounts.push(Mount {
                                    key,
                                    role,
                                    path: input,
                                });
                            }
                        }
                    }
                }
                self.notice=Some("Configuration inspected. Project env files are read directly. Confirm database names, then save the project profile.".into());
            }
            Event::Saved(message) => {
                self.notice = Some(message);
                self.fill_database_names(cx);
            }
            Event::Configured(spec) => {
                if let Some(names) = &spec.automatic_connections {
                    if self.mysql_database.read(cx).value().is_empty() {
                        self.mysql_database
                            .update(cx, |field, cx| field.set_value(names.mysql.clone(), cx));
                    }
                    if self.ch_database.read(cx).value().is_empty() {
                        self.ch_database.update(cx, |field, cx| {
                            field.set_value(names.clickhouse.clone(), cx)
                        });
                    }
                }
                self.spec = spec;
                self.notice = Some(
                    "Project profile saved. Start sandbox prepares everything automatically."
                        .into(),
                );
            }
        }
    }
    fn create_project(&mut self, cx: &mut Context<Self>) {
        let name = self.project_name.read(cx).value();
        let location = self.location.read(cx).value();
        let clone = self.clone_remote;
        let storage = self
            .app
            .catalog
            .project(&self.project)
            .map(|p| p.storage)
            .unwrap_or_default();
        self.run(
            "Add project",
            move |app, cancel| {
                let project = app.add_project(name, location, clone, storage, &cancel)?;
                Ok(Event::Created(project.id, None))
            },
            cx,
        );
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.sandbox.clone() else {
            return;
        };
        self.run("Discover Compose files",move |app,cancel|{let sandbox=app.catalog.sandbox(&id)?;let source=sandbox.source.ok_or_else(||anyhow::anyhow!("This sandbox contains databases only. Create a sandbox with a Git source to run the project."))?;Ok(Event::Discovered(compose::discover(&source.directory,&cancel)?))},cx);
    }
    fn inspect(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.sandbox.clone() else {
            return;
        };
        let spec = self.spec.clone();
        self.run(
            "Inspect Compose configuration",
            move |app, cancel| {
                let sandbox = app.catalog.sandbox(&id)?;
                let source = sandbox
                    .source
                    .ok_or_else(|| anyhow::anyhow!("Select a source first"))?;
                let env = app.catalog.environment(&sandbox.project_id, "*")?;
                let document = compose::inspect(&source.directory, &spec, &env, &cancel, false)?;
                let mounts = sandbox_projects::launch_profile::infer_mounts(
                    &source.directory,
                    &document,
                    &cancel,
                )?;
                Ok(Event::Inspected(document, mounts))
            },
            cx,
        );
    }
    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.sandbox.clone() else {
            return;
        };
        let mut spec = self.spec.clone();
        spec.automatic_connections = Some(DatabaseNames {
            mysql: self.mysql_database.read(cx).value().trim().to_owned(),
            clickhouse: self.ch_database.read(cx).value().trim().to_owned(),
        });
        let service = self.probe_service.read(cx).value().trim().to_owned();
        spec.readiness_probe = if service.is_empty() {
            None
        } else {
            let Ok(port) = self.probe_port.read(cx).value().trim().parse::<u16>() else {
                self.save_error = Some("Enter a valid HTTP readiness container port".into());
                cx.notify();
                return;
            };
            Some(ReadinessProbe {
                service,
                port,
                path: self.probe_path.read(cx).value().trim().to_owned(),
            })
        };
        spec.databases.clear();
        for (role, input) in [("mysql", &self.mysql), ("clickhouse", &self.clickhouse)] {
            let name = input.read(cx).value();
            if name.trim().is_empty() {
                self.save_error = Some("Enter both database service names".into());
                cx.notify();
                return;
            }
            if spec
                .databases
                .insert(name, DatabaseId::try_from(role.to_owned()).unwrap())
                .is_some()
            {
                self.save_error = Some("Database service names must differ".into());
                cx.notify();
                return;
            }
        }
        for mount in &self.mounts {
            spec.mounts.insert(
                mount.key.clone(),
                match mount.role.read(cx).value() {
                    "source" => MountRole::Source,
                    "config" => MountRole::Configuration,
                    _ => MountRole::Data,
                },
            );
            spec.mount_paths
                .insert(mount.key.clone(), mount.path.read(cx).value().into());
        }
        self.spec = spec.clone();
        self.run(
            "Save project launch profile",
            move |app, cancel| {
                Ok(Event::Configured(
                    app.save_launch_profile(&id, spec, &cancel)?,
                ))
            },
            cx,
        );
    }
    fn import(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let service = self.env_service.read(cx).value().to_owned();
        let mut path = PathBuf::from(self.env_path.read(cx).value());
        if path.is_relative()
            && let Some(source) = self
                .sandbox
                .as_ref()
                .and_then(|id| self.app.catalog.sandbox(id).ok())
                .and_then(|s| s.source)
        {
            path = source.directory.join(path);
        }
        self.run("Import environment into SQLite",move |app,_|{let count=compose::import_environment(&app.catalog,&app.catalog.project(&project)?,&service,&path)?;Ok(Event::Saved(format!("Imported {count} values into application storage. The source file is unchanged.")))},cx);
    }
    fn binding(&mut self, cx: &mut Context<Self>) {
        let service = self.binding_service.read(cx).value().to_owned();
        let variable = self.variable.read(cx).value();
        if !self.services.contains(&service) || variable.trim().is_empty() {
            self.binding_error = Some("Choose a service and enter its environment variable".into());
            cx.notify();
            return;
        }
        let parameter = match self.binding_parameter.read(cx).value() {
            "port" => DatabaseParameter::Port,
            "user" => DatabaseParameter::User,
            "password" => DatabaseParameter::Password,
            "database" => DatabaseParameter::Database,
            "url" => DatabaseParameter::Url,
            _ => DatabaseParameter::Host,
        };
        self.spec
            .bindings
            .retain(|b| b.service != service || b.variable != variable);
        self.spec.bindings.push(DatabaseBinding {
            service,
            variable,
            database: DatabaseId::try_from(self.binding_db.read(cx).value().to_owned()).unwrap(),
            parameter,
            database_name: self.database_name.read(cx).value(),
        });
        self.binding_error = None;
        cx.notify();
    }
}
fn row(label: impl Into<SharedString>, control: impl IntoElement, p: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .text_sm()
                .text_color(rgb(p.text_muted))
                .child(label.into()),
        )
        .child(control)
}
fn heading(label: &'static str, p: Palette) -> Div {
    div()
        .mt_6()
        .pb_2()
        .border_b_1()
        .border_color(rgb(p.border))
        .text_lg()
        .child(label)
}
fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    p: Palette,
    cx: &Context<ProjectsView>,
    action: impl Fn(&mut ProjectsView, &mut Window, &mut Context<ProjectsView>) + 'static,
) -> Stateful<Div> {
    let action = std::rc::Rc::new(action);
    let keyboard = action.clone();
    div()
        .id(id)
        .h(px(28.))
        .px_2()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(p.border))
        .text_sm()
        .when(!enabled, |d| d.opacity(0.45))
        .when(enabled, |d| {
            d.tab_index(0)
                .cursor_pointer()
                .hover(|d| d.bg(rgb(p.element_hover)))
                .focus(|d| d.border_color(rgb(p.border_focused)))
                .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        keyboard(this, window, cx);
                        cx.stop_propagation();
                    }
                }))
        })
        .child(label.into())
}
impl Render for ProjectsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.form_focus.take().is_some() {
            self.project_name.read(cx).focus(window);
        }

        let p = self.theme.palette(window.appearance());
        let ready = self.operation.is_none() && !self.app.is_stopping();
        let sandbox = self
            .sandbox
            .as_ref()
            .and_then(|id| self.app.catalog.sandbox(id).ok());
        let project = self.app.catalog.project(&self.project).ok();
        let mut page = div()
            .id("projects-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_3()
            .text_color(rgb(p.text))
            .child(div().text_xl().child("Project settings"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(p.text_muted))
                    .child(project.as_ref().map(|p| p.name.clone()).unwrap_or_default()),
            );
        page = page.child(
            div()
                .flex()
                .gap_1()
                .children(
                    [(0, "Project"), (2, "Launch")]
                        .into_iter()
                        .map(|(index, label)| {
                            button(
                                ("setup-tab", index),
                                label,
                                ready,
                                p,
                                cx,
                                move |this, _, cx| {
                                    this.tab = index;
                                    cx.notify();
                                },
                            )
                            .when(self.tab == index, |d| d.bg(rgb(p.element_selected)))
                        }),
                ),
        );
        if let Some(error) = &self.error {
            page = page.child(
                div()
                    .text_sm()
                    .text_color(rgb(p.error))
                    .child(error.clone()),
            );
        }
        if let Some(notice) = &self.notice {
            page = page.child(div().text_sm().child(notice.clone()));
        }
        if let Some(job) = &self.operation {
            page = page.child(
                div()
                    .flex()
                    .gap_3()
                    .items_center()
                    .child(job.name.clone())
                    .child(button(
                        "cancel-setup",
                        "Cancel",
                        true,
                        p,
                        cx,
                        |this, _, cx| {
                            if let Some(job) = &this.operation {
                                job.cancel();
                            }
                            cx.notify();
                        },
                    )),
            );
        }
        if self.tab == 0 {
            page = page
                .child(heading("Add project", p))
                .child(row("Name", self.project_name.clone(), p))
                .child(row(
                    if self.clone_remote {
                        "Git repository URL"
                    } else {
                        "Local Git directory (optional)"
                    },
                    self.location.clone(),
                    p,
                ))
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .flex_wrap()
                        .child(button(
                            "repo-mode",
                            if self.clone_remote {
                                "Clone remote repository"
                            } else {
                                "Use local repository"
                            },
                            ready,
                            p,
                            cx,
                            |this, _, cx| {
                                this.clone_remote = !this.clone_remote;
                                cx.notify();
                            },
                        ))
                        .child(button(
                            "add-project",
                            "Add project",
                            ready,
                            p,
                            cx,
                            |this, _, cx| this.create_project(cx),
                        )),
                )
                .child(heading("Backup locations", p))
                .child(div().text_sm().text_color(rgb(p.text_muted)).child(
                    "S3 connection and credentials: Settings → Storage for the selected project.",
                ))
                .child(row("MySQL prefix", self.mysql_prefix.clone(), p))
                .child(row("ClickHouse prefix", self.ch_prefix.clone(), p))
                .child(div().flex().child(button(
                    "save-prefixes",
                    "Save backup prefixes",
                    ready,
                    p,
                    cx,
                    |this, _, cx| {
                        let result = (|| {
                            let mut project = this.app.catalog.project(&this.project)?;
                            for (id, field) in [
                                ("mysql", &this.mysql_prefix),
                                ("clickhouse", &this.ch_prefix),
                            ] {
                                let prefix = field.read(cx).value();
                                anyhow::ensure!(
                                    !prefix.trim().is_empty(),
                                    "Enter both backup prefixes"
                                );
                                project
                                    .databases
                                    .iter_mut()
                                    .find(|(key, _)| key.as_str() == id)
                                    .unwrap()
                                    .1
                                    .prefix = prefix;
                            }
                            this.app
                                .backups(&project.id)?
                                .configure_prefixes(project.databases)?;
                            Ok::<_, anyhow::Error>(())
                        })();
                        match result {
                        Ok(()) => this.notice = Some(
                            "Backup prefixes saved. Refresh Backups to check the new locations."
                                .into(),
                        ),
                        Err(e) => this.error = Some(e.to_string()),
                    }
                        cx.notify();
                    },
                )));
        }
        if self.tab == 2 && sandbox.is_none() {
            page = page.child(
                div()
                    .text_sm()
                    .text_color(rgb(p.text_muted))
                    .child("Choose a working copy in the titlebar to configure its launch."),
            );
        }
        if self.tab == 2
            && let Some(sandbox) = sandbox
        {
            page = page
                .child(heading("Project launch profile", p))
                .child(div().child(sandbox.name));
            if self.remove_confirmation {
                page=page.child(div().text_sm().text_color(rgb(p.error)).child("Remove this sandbox and all of its database/application data? Clean app-owned worktrees will be removed; user directories and branches are kept."))
                    .child(div().flex().gap_2().child(button("cancel-remove","Cancel",ready,p,cx,|this,_,cx|{this.remove_confirmation=false;cx.notify();})).child(button("confirm-remove","Remove sandbox",ready,p,cx,|this,_,cx|{if let Some(id)=this.sandbox.clone(){let project=this.project.clone();this.remove_confirmation=false;this.run("Remove sandbox",move |app,cancel|{app.remove_sandbox(&id,&cancel)?;Ok(Event::Created(project,None))},cx);}}).text_color(rgb(p.error))));
            } else {
                page = page.child(
                    div().flex().child(
                        button(
                            "request-remove",
                            "Remove sandbox…",
                            ready,
                            p,
                            cx,
                            |this, _, cx| {
                                this.remove_confirmation = true;
                                cx.notify();
                            },
                        )
                        .text_color(rgb(p.error)),
                    ),
                );
            }

            if let Some(source) = sandbox.source {
                page = page
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(p.text_muted))
                            .child(source.directory.display().to_string()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button(
                                "discover",
                                "Find Compose files",
                                ready,
                                p,
                                cx,
                                |this, _, cx| this.discover(cx),
                            ))
                            .child(button(
                                "inspect",
                                "Inspect selection",
                                ready && !self.spec.files.is_empty(),
                                p,
                                cx,
                                |this, _, cx| this.inspect(cx),
                            )),
                    );
                for (index, candidate) in self.candidates.iter().enumerate() {
                    let path = candidate.path.clone();
                    let selected = self.spec.files.iter().position(|v| v == &path);
                    let label = format!(
                        "{}{}",
                        selected.map(|i| format!("{}. ", i + 1)).unwrap_or_default(),
                        path.display()
                    );
                    page = page.child(button(
                        ("compose-file", index),
                        label,
                        ready,
                        p,
                        cx,
                        move |this, _, cx| {
                            if let Some(index) = this.spec.files.iter().position(|v| v == &path) {
                                this.spec.files.remove(index);
                            } else {
                                this.spec.files.push(path.clone());
                            }
                            cx.notify();
                        },
                    ));
                }
                if !self.spec.files.is_empty() {
                    page =
                        page.child(div().text_sm().text_color(rgb(p.text_muted)).child(format!(
                            "Merge order: {}",
                            self.spec
                                .files
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(" → ")
                        )));
                }
                page=page.child(heading("Databases",p))
                    .child(row("MySQL database (DB_NAME)",self.mysql_database.clone(),p))
                    .child(row("ClickHouse database",self.ch_database.clone(),p))
                    .child(div().text_sm().text_color(rgb(p.text_muted)).child("Names are inferred from project env files when available. Hosts, ports, logins and passwords are managed by the sandbox."))
                    .child(button("advanced-launch",if self.advanced{"Hide advanced settings"}else{"Advanced settings…"},ready,p,cx,|this,_,cx|{this.advanced=!this.advanced;cx.notify();}));
                if self.advanced {
                    page = page
                        .child(heading("Optional environment overrides", p))
                        .child(div().text_sm().text_color(rgb(p.text_muted)).child("Compose reads the project's env_file directly. Import only to keep explicit overrides in application storage; existing imports remain active."))
                        .child(row("Env file to import", self.env_path.clone(), p))
                        .child(row("Import environment for", self.env_service.clone(), p))
                        .child(div().flex().child(button("import-env", "Import overrides into application storage", ready, p, cx, |this, _, cx| this.import(cx))))
                        .child(heading("Optional HTTP readiness check", p))
                        .child(row("Application service", self.probe_service.clone(), p))
                        .child(row("Published container port", self.probe_port.clone(), p))
                        .child(row(
                            "HTTP path (for example /health)",
                            self.probe_path.clone(),
                            p,
                        ));
                    page = page.child(div().text_sm().child("Profiles"));
                    for (index, profile) in self.profiles.iter().enumerate() {
                        let value = profile.clone();
                        page = page.child(button(
                            ("profile", index),
                            format!(
                                "{}{}",
                                if self.spec.profiles.contains(profile) {
                                    "Selected · "
                                } else {
                                    ""
                                },
                                profile
                            ),
                            ready,
                            p,
                            cx,
                            move |this, _, cx| {
                                toggle(&mut this.spec.profiles, &value);
                                cx.notify();
                            },
                        ));
                    }
                    page = page.child(
                        div()
                            .text_sm()
                            .child("Services (empty selection uses enabled profiles)"),
                    );
                    for (index, service) in self.services.iter().enumerate() {
                        let value = service.clone();
                        page = page.child(button(
                            ("service", index),
                            format!(
                                "{}{}",
                                if self.spec.services.contains(service) {
                                    "Selected · "
                                } else {
                                    ""
                                },
                                service
                            ),
                            ready,
                            p,
                            cx,
                            move |this, _, cx| {
                                toggle(&mut this.spec.services, &value);
                                cx.notify();
                            },
                        ));
                    }
                    page = page
                        .child(row("MySQL service to replace", self.mysql.clone(), p))
                        .child(row(
                            "ClickHouse service to replace",
                            self.clickhouse.clone(),
                            p,
                        ))
                        .child(heading("Database bindings", p));
                    for (index, binding) in self.spec.bindings.iter().enumerate() {
                        page = page.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(div().min_w_0().text_sm().child(format!(
                                    "{} · {} ← {} {:?}",
                                    binding.service,
                                    binding.variable,
                                    binding.database,
                                    binding.parameter
                                )))
                                .child(button(
                                    ("remove-binding", index),
                                    "Remove",
                                    ready,
                                    p,
                                    cx,
                                    move |this, _, cx| {
                                        this.spec.bindings.remove(index);
                                        cx.notify();
                                    },
                                )),
                        );
                    }
                    page = page
                        .child(row("Service", self.binding_service.clone(), p))
                        .child(row("Environment variable", self.variable.clone(), p))
                        .child(row("Database", self.binding_db.clone(), p))
                        .child(row("Value", self.binding_parameter.clone(), p))
                        .child(row(
                            "Database name (for Database / URL)",
                            self.database_name.clone(),
                            p,
                        ))
                        .child(div().flex().child(button(
                            "add-binding",
                            "Add binding",
                            ready,
                            p,
                            cx,
                            |this, _, cx| this.binding(cx),
                        )));
                    if let Some(error) = &self.binding_error {
                        page = page.child(
                            div()
                                .text_sm()
                                .text_color(rgb(p.error))
                                .child(error.clone()),
                        );
                    }
                    if !self.mounts.is_empty() {
                        page = page.child(heading("Bind mounts", p));
                        for mount in &self.mounts {
                            page = page
                                .child(row(mount.key.clone(), mount.role.clone(), p))
                                .child(row("Source / configuration path", mount.path.clone(), p));
                        }
                    }
                    page = page.child(div().flex().child(button(
                        "external-resources",
                        if self.spec.private_external_resources {
                            "Private replacements for external resources: enabled"
                        } else {
                            "Allow private replacements for external resources"
                        },
                        ready,
                        p,
                        cx,
                        |this, _, cx| {
                            this.spec.private_external_resources =
                                !this.spec.private_external_resources;
                            cx.notify();
                        },
                    )));
                }
                page = page
                    .when_some(self.save_error.clone(), |d, error| {
                        d.child(div().text_sm().text_color(rgb(p.error)).child(error))
                    })
                    .child(div().flex().child(button(
                        "save-compose",
                        "Save project profile",
                        ready,
                        p,
                        cx,
                        |this, _, cx| this.save(cx),
                    )));
            } else {
                page=page.child(div().text_sm().text_color(rgb(p.text_muted)).child("Legacy database-only environment. Choose a working copy in the titlebar to run project services."));
            }
        }
        div().flex().flex_col().size_full().min_h_0().child(page)
    }
}
fn toggle(values: &mut Vec<String>, value: &str) {
    if let Some(index) = values.iter().position(|v| v == value) {
        values.remove(index);
    } else {
        values.push(value.into());
    }
}
