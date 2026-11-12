//! Compose is the parser of record; generated files contain references, not secrets.
use anyhow::{Context, Result, ensure};
use sandbox_core::models::*;
use sandbox_platform::{
    catalog::Catalog,
    docker::{Docker, strings},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

#[derive(Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub services: Vec<String>,
    pub profiles: Vec<String>,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Plan {
    pub document: Value,
    pub environment: BTreeMap<String, String>,
    pub application_services: Vec<String>,
    pub managed_services: BTreeMap<String, String>,
    #[serde(default)]
    pub connection_databases: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub readiness_probe: Option<ReadinessProbe>,
}
impl Plan {
    pub fn detach_backup(&mut self) -> Result<()> {
        fn decode(value: &mut Value, environment: &BTreeMap<String, String>) {
            match value {
                Value::String(text) => {
                    if let Some(name) = text.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
                        && let Some(value) = environment.get(name)
                    {
                        *text = value.clone();
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        decode(item, environment)
                    }
                }
                Value::Object(items) => {
                    for value in items.values_mut() {
                        decode(value, environment)
                    }
                }
                _ => {}
            }
        }
        let mut doc = self.document.clone();
        decode(&mut doc, &self.environment);
        if let Some(name) = self.managed_services.get("clickhouse")
            && let Some(mounts) = doc["services"][name]["volumes"].as_array_mut()
        {
            mounts.retain(|mount| mount["target"] != "/backups/clickhouse.tar.zst");
        }
        let connections = self.connection_databases.clone();
        let probe = self.readiness_probe.clone();
        *self = protected(
            doc,
            self.application_services.clone(),
            self.managed_services.clone(),
        )?;
        self.connection_databases = connections;
        self.readiness_probe = probe;
        Ok(())
    }
}

fn scope_label(kind: &str) -> String {
    format!("ru.mentormarketplace.sandbox-prepare.{kind}")
}
fn args(directory: &Path, spec: &ComposeLaunchSpec) -> Result<Vec<String>> {
    ensure!(!spec.files.is_empty(), "Choose at least one Compose file");
    let mut args = strings(&[
        "compose",
        "--project-directory",
        &directory.display().to_string(),
        "--project-name",
        "sandbox-inspect",
        "--profile",
        "*",
    ]);
    for file in &spec.files {
        ensure!(
            !file.is_absolute() && !file.components().any(|p| matches!(p, Component::ParentDir)),
            "Compose files must be inside the working directory"
        );
        args.extend(strings(&[
            "--file",
            &directory.join(file).display().to_string(),
        ]));
    }
    args.extend(strings(&[
        "config",
        "--no-env-resolution",
        "--no-consistency",
        "--format",
        "json",
    ]));
    Ok(args)
}
pub fn inspect(
    directory: &Path,
    spec: &ComposeLaunchSpec,
    environment: &BTreeMap<String, String>,
    cancel: &AtomicBool,
    interpolate: bool,
) -> Result<Value> {
    let mut command = args(directory, spec)?;
    if !interpolate {
        command.push("--no-interpolate".into());
    }
    let output =
        Docker::discover().capture(&command, environment, cancel, Duration::from_secs(60))?;
    let mut document = serde_json::from_str(&output).context("Compose returned invalid JSON")?;
    if interpolate {
        decode_export(&mut document);
    }
    Ok(document)
}
/// Resolve project env files with Compose itself for inference and consumer detection.
/// Values are transient: the generated plan keeps env_file references, not their contents.
pub fn inspect_environment(
    directory: &Path,
    spec: &ComposeLaunchSpec,
    environment: &BTreeMap<String, String>,
    cancel: &AtomicBool,
) -> Result<Value> {
    let mut command = args(directory, spec)?;
    command.retain(|arg| arg != "--no-env-resolution");
    let output =
        Docker::discover().capture(&command, environment, cancel, Duration::from_secs(60))?;
    let mut document = serde_json::from_str(&output).context("Compose returned invalid JSON")?;
    decode_export(&mut document);
    if let Some(services) = document["services"].as_object() {
        for service in services.keys() {
            register_secrets(&service_environment(&document, service));
        }
    }
    Ok(document)
}
pub(crate) fn service_environment(document: &Value, name: &str) -> BTreeMap<String, String> {
    document["services"][name]["environment"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
        .collect()
}
// `compose config` exports a reusable Compose document, doubling literal dollars.
// Our plan stores runtime values separately, so decode that serialization exactly once.
fn decode_export(value: &mut Value) {
    match value {
        Value::String(text) => *text = text.replace("$$", "$"),
        Value::Array(items) => items.iter_mut().for_each(decode_export),
        Value::Object(items) => items.values_mut().for_each(decode_export),
        _ => {}
    }
}
pub fn discover(directory: &Path, cancel: &AtomicBool) -> Result<Vec<Candidate>> {
    let output = crate::git::Git::default().run(
        directory,
        &strings(&[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            "*.yml",
            "*.yaml",
        ]),
        cancel,
    )?;
    let mut candidates = Vec::new();
    for file in output.split('\0').filter(|file| !file.is_empty()) {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "Discovery cancelled"
        );
        let path = PathBuf::from(file);
        let spec = ComposeLaunchSpec {
            files: vec![path.clone()],
            ..Default::default()
        };
        if let Ok(doc) = inspect(directory, &spec, &BTreeMap::new(), cancel, false) {
            let services = service_names(&doc);
            if !services.is_empty() {
                let profiles = profile_names(&doc);
                candidates.push(Candidate {
                    path,
                    services,
                    profiles,
                });
            }
        }
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(candidates)
}
pub(crate) fn included_services(
    source: &serde_json::Map<String, Value>,
    spec: &ComposeLaunchSpec,
    managed_names: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let selected: Vec<String> = if spec.services.is_empty() {
        source
            .iter()
            .filter(|(name, value)| {
                !managed_names.contains(*name)
                    && value["profiles"].as_array().is_none_or(|profiles| {
                        profiles
                            .iter()
                            .filter_map(Value::as_str)
                            .any(|p| spec.profiles.iter().any(|selected| selected == p))
                    })
            })
            .map(|(name, _)| name.clone())
            .collect()
    } else {
        spec.services
            .iter()
            .filter(|name| !managed_names.contains(*name))
            .cloned()
            .collect()
    };
    ensure!(
        !selected.is_empty(),
        "Choose application services or enable their Compose profiles"
    );
    let mut included = managed_names.clone();
    let mut queue = selected.clone();
    while let Some(name) = queue.pop() {
        if !included.insert(name.clone()) {
            continue;
        }
        let service = source
            .get(&name)
            .with_context(|| format!("Unknown Compose service: {name}"))?;
        if let Some(depends) = service["depends_on"].as_object() {
            queue.extend(depends.keys().cloned());
        }
        if let Some(depends) = service["depends_on"].as_array() {
            queue.extend(depends.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        for key in ["links", "volumes_from"] {
            if let Some(values) = service[key].as_array() {
                queue.extend(
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.split(':').next().unwrap_or(s).to_owned()),
                );
            }
        }
        for key in ["network_mode", "ipc", "pid"] {
            if let Some(name) = service[key]
                .as_str()
                .and_then(|v| v.strip_prefix("service:"))
            {
                queue.push(name.into());
            }
        }
    }
    Ok(included)
}
pub fn service_names(document: &Value) -> Vec<String> {
    document["services"]
        .as_object()
        .map(|s| s.keys().cloned().collect())
        .unwrap_or_default()
}
pub fn profile_names(document: &Value) -> Vec<String> {
    let mut names = BTreeSet::new();
    if let Some(services) = document["services"].as_object() {
        for service in services.values() {
            if let Some(profiles) = service["profiles"].as_array() {
                for name in profiles.iter().filter_map(Value::as_str) {
                    names.insert(name.to_owned());
                }
            }
        }
    }
    names.into_iter().collect()
}
pub fn import_environment(
    catalog: &Catalog,
    project: &Project,
    service: &str,
    path: &Path,
) -> Result<usize> {
    let values = dotenvy::from_read_iter(fs::File::open(path)?)
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
    register_secrets(&values);
    let count = values.len();
    catalog.save_environment(&project.id, service, &values)?;
    Ok(count)
}
fn register_secrets(values: &BTreeMap<String, String>) {
    for (key, value) in values {
        let key = key.to_ascii_lowercase();
        if ["pass", "secret", "token", "key", "url", "dsn"]
            .iter()
            .any(|part| key.contains(part))
        {
            sandbox_telemetry::logging::register_credentials(value, "");
        }
    }
}

pub fn build(
    catalog: &Catalog,
    project: &Project,
    sandbox: &Sandbox,
    generation: &str,
    managed: Value,
    cancel: &AtomicBool,
) -> Result<Plan> {
    let Some(spec) = sandbox
        .compose
        .as_ref()
        .filter(|spec| !spec.files.is_empty())
    else {
        return protected(
            managed,
            vec![],
            BTreeMap::from([
                ("mysql".into(), "mysql".into()),
                ("clickhouse".into(), "clickhouse".into()),
            ]),
        );
    };
    let directory = &sandbox
        .source
        .as_ref()
        .context("Choose the source checkout first")?
        .directory;
    let mut global = catalog.environment(&project.id, "*")?;
    let imported_global = global.clone();
    let automatic = spec.automatic_connections.as_ref().map(|names| {
        let role = |kind: &'static str| {
            spec.databases
                .iter()
                .find(|(_, id)| id.as_str() == kind)
                .map(|(name, _)| name.as_str())
                .unwrap_or(kind)
        };
        crate::launch_profile::environment(
            names,
            role("mysql"),
            role("clickhouse"),
            &sandbox.password,
        )
    });
    if let Some(values) = &automatic {
        global.extend(values.clone());
    }
    register_secrets(&global);
    let resolved_environment = inspect_environment(directory, spec, &global, cancel)?;
    let mut document = inspect(directory, spec, &global, cancel, true)?;
    let source = document["services"]
        .as_object_mut()
        .context("Compose has no services")?;
    let mut roles = BTreeMap::new();
    for role in ["mysql", "clickhouse"] {
        let name = spec
            .databases
            .iter()
            .find(|(_, id)| id.as_str() == role)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| role.into());
        ensure!(
            !roles.values().any(|existing| existing == &name),
            "Database services must have different names"
        );
        source.insert(name.clone(), managed["services"][role].clone());
        roles.insert(role.into(), name);
    }
    let managed_names: BTreeSet<_> = roles.values().cloned().collect();
    let included = included_services(source, spec, &managed_names)?;
    source.retain(|name, _| included.contains(name));
    let applications: Vec<_> = source
        .keys()
        .filter(|name| !managed_names.contains(*name))
        .cloned()
        .collect();
    let mut connection_databases = BTreeMap::<String, BTreeSet<String>>::new();
    let mut db_networks = BTreeMap::<String, BTreeSet<String>>::new();
    for name in &applications {
        let service = source.get_mut(name).unwrap();
        let object = service.as_object_mut().context("Invalid service")?;
        object.remove("profiles");
        ensure!(
            object
                .get("external_links")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
            "Service {name} uses external_links; replace these with sandbox services before launching"
        );
        ensure!(
            object
                .get("network_mode")
                .and_then(Value::as_str)
                .is_none_or(|mode| mode != "host"),
            "Service {name} uses host networking; configure an isolated network before launching"
        );
        object.insert(
            "container_name".into(),
            json!(format!(
                "{}-app-{name}",
                managed["name"]
                    .as_str()
                    .context("Managed Compose project has no name")?
            )),
        );
        object.insert("restart".into(), json!("no"));
        if let Some(ports) = object.get_mut("ports").and_then(Value::as_array_mut) {
            for port in ports {
                ensure!(
                    port.is_object(),
                    "Service {name} has an unsupported port definition"
                );
                port["host_ip"] = json!("127.0.0.1");
                port["published"] = json!("0");
            }
        }

        let labels = object
            .entry("labels")
            .or_insert(json!({}))
            .as_object_mut()
            .context("Invalid labels")?;
        for (key, value) in [
            ("owner", catalog.owner.as_str()),
            ("project", project.id.as_str()),
            ("sandbox", sandbox.id.as_str()),
            ("instance", generation),
            (
                "session",
                managed["services"]["mysql"]["labels"][scope_label("session")]
                    .as_str()
                    .context("Managed services lack an application session")?,
            ),
        ] {
            labels.insert(scope_label(key), json!(value));
        }
        labels.insert(scope_label("role"), json!("application"));
        let mut environment = BTreeMap::new();
        if object.get("env_file").is_some() {
            // Retain previously imported values as optional explicit overrides.
            environment.extend(imported_global.clone());
        }
        environment.extend(catalog.environment(&project.id, name)?);
        if let Some(explicit) = object.get("environment").and_then(Value::as_object) {
            for (key, value) in explicit {
                if let Some(value) = value.as_str() {
                    environment.insert(key.clone(), value.into());
                } else if !value.is_null() {
                    environment.insert(key.clone(), value.to_string());
                }
            }
        }
        let null_variables: Vec<_> = object
            .get("environment")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(_, value)| value.is_null())
            .map(|(key, _)| key.clone())
            .collect();
        let mut effective = service_environment(&resolved_environment, name);
        effective.extend(environment.clone());
        for key in &null_variables {
            effective.remove(key);
        }
        if let Some(values) = &automatic {
            let mysql = crate::launch_profile::uses_mysql(&effective);
            let ch = crate::launch_profile::uses_clickhouse(&effective);
            for (key, value) in values {
                if (mysql && key.starts_with("DB_")) || (ch && key.starts_with("CLICKHOUSE_")) {
                    environment.insert(key.clone(), value.clone());
                }
            }
            for (used, role, key) in [
                (mysql, "mysql", "DB_NAME"),
                (ch, "clickhouse", "CLICKHOUSE_DATABASE"),
            ] {
                if used {
                    let name = values.get(key).context("Missing automatic database name")?;
                    ensure!(!name.trim().is_empty(), "Set {key} in the project profile");
                    connection_databases
                        .entry(role.into())
                        .or_default()
                        .insert(name.clone());
                    db_networks.entry(role.into()).or_default().extend(
                        object
                            .get("networks")
                            .and_then(Value::as_object)
                            .into_iter()
                            .flat_map(|n| n.keys().cloned()),
                    );
                }
            }
        }
        register_secrets(&environment);
        let mut final_environment = serde_json::to_value(&environment)?;
        for key in null_variables {
            if !environment.contains_key(&key) {
                final_environment[&key] = Value::Null;
            }
        }
        object.insert("environment".into(), final_environment);
        if let Some(mounts) = object.get_mut("volumes").and_then(Value::as_array_mut) {
            for mount in mounts {
                if mount["type"] != "bind" {
                    continue;
                }
                let target = mount["target"]
                    .as_str()
                    .context("Bind mount lacks a target")?
                    .to_owned();
                let key = format!("{name}:{target}");
                let original = mount["source"]
                    .as_str()
                    .context("Bind mount lacks a source")?;
                let original = PathBuf::from(original);
                let default = if mount["read_only"].as_bool() == Some(true) {
                    MountRole::Configuration
                } else if original == *directory {
                    MountRole::Source
                } else {
                    MountRole::Data
                };
                let role = spec.mounts.get(&key).unwrap_or(&default);
                let path = if matches!(role, MountRole::Data) {
                    sandbox.root.join("application-data").join(hex_name(&key))
                } else {
                    let path = spec.mount_paths.get(&key).cloned().unwrap_or(original);
                    if path.is_relative() {
                        directory.join(path)
                    } else {
                        path
                    }
                };
                if matches!(role, MountRole::Data) {
                    fs::create_dir_all(&path)?;
                } else {
                    ensure!(
                        path.exists(),
                        "Mount {} is missing: {}. Choose an existing source/configuration path.",
                        key,
                        path.display()
                    );
                }
                mount["source"] = json!(path);
                if matches!(role, MountRole::Configuration) {
                    mount["read_only"] = json!(true);
                }
            }
        }
    }
    for binding in &spec.bindings {
        let role = binding.database.as_str();
        if matches!(
            binding.parameter,
            DatabaseParameter::Database | DatabaseParameter::Url
        ) && !binding.database_name.is_empty()
        {
            connection_databases
                .entry(role.into())
                .or_default()
                .insert(binding.database_name.clone());
        }
        let host = roles
            .get(role)
            .context("Binding refers to an unknown database")?;
        let target = source.get_mut(&binding.service).with_context(|| {
            format!(
                "Binding refers to an unselected service: {}",
                binding.service
            )
        })?;
        ensure!(
            !binding.variable.trim().is_empty(),
            "Enter a connection variable name"
        );
        let port = if role == "mysql" { "3306" } else { "8123" };
        let value = match binding.parameter {
            DatabaseParameter::Host => host.clone(),
            DatabaseParameter::Port => port.into(),
            DatabaseParameter::User => "sandbox".into(),
            DatabaseParameter::Password => sandbox.password.clone(),
            DatabaseParameter::Database => binding.database_name.clone(),
            DatabaseParameter::Url => {
                let scheme = if role == "mysql" { "mysql" } else { "http" };
                format!(
                    "{scheme}://sandbox:{}@{host}:{port}/{}",
                    sandbox.password, binding.database_name
                )
            }
        };
        target["environment"]
            .as_object_mut()
            .context("Service environment is invalid")?
            .insert(binding.variable.clone(), json!(value));
        let networks: Vec<_> = target["networks"]
            .as_object()
            .map(|n| n.keys().cloned().collect())
            .unwrap_or_else(|| vec!["default".into()]);
        db_networks.entry(role.into()).or_default().extend(networks);
    }
    for (role, name) in &roles {
        let service = source.get_mut(name).unwrap();
        let networks = db_networks.remove(role).unwrap_or_default();
        let mut networks: serde_json::Map<String, Value> =
            networks.into_iter().map(|name| (name, json!({}))).collect();
        networks.insert("default".into(), json!({}));
        service["networks"] = Value::Object(networks);
        let command = if role == "mysql" {
            "mysql --defaults-extra-file=/tmp/sandbox-client.cnf --batch -e 'SELECT 1'"
        } else {
            "clickhouse-client --config-file=/tmp/sandbox-client.xml --query 'SELECT 1'"
        };
        service["healthcheck"] =
            json!({"test":["CMD-SHELL",command],"interval":"2s","timeout":"5s","retries":90});
    }
    let mut networks = document
        .get("networks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    networks.entry("default").or_insert(json!({}));
    for (name, network) in &mut networks {
        ensure!(
            network["external"] != true || spec.private_external_resources,
            "Network {name} is external; choose a private replacement"
        );
        let object = network.as_object_mut().context("Invalid network")?;
        object.remove("external");
        object.insert(
            "name".into(),
            json!(format!(
                "sp-{}-{generation}-{}",
                &catalog.owner[..12],
                hex_name(name)
            )),
        );
        object.insert("labels".into(),json!({scope_label("owner"):catalog.owner,scope_label("sandbox"):sandbox.id.as_str(),scope_label("instance"):generation}));
    }
    let mut volumes = document
        .get("volumes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, volume) in &mut volumes {
        ensure!(
            volume["external"] != true || spec.private_external_resources,
            "Volume {name} is external; choose a private replacement"
        );
        let object = volume.as_object_mut().context("Invalid volume")?;
        object.remove("external");
        ensure!(
            object
                .get("driver_opts")
                .is_none_or(|v| v.as_object().is_some_and(serde_json::Map::is_empty)),
            "Volume {name} uses driver options that could reference shared data; replace it with a private local volume"
        );
        ensure!(
            object
                .get("driver")
                .and_then(Value::as_str)
                .is_none_or(|v| v == "local"),
            "Volume {name} uses a shared storage driver"
        );
        object.insert(
            "name".into(),
            json!(format!(
                "sp-{}-{}-{}",
                &catalog.owner[..12],
                sandbox.id,
                hex_name(name)
            )),
        );
        object.insert(
            "labels".into(),
            json!({scope_label("owner"):catalog.owner,scope_label("sandbox"):sandbox.id.as_str()}),
        );
    }
    if let Some(managed_volumes) = managed["volumes"].as_object() {
        volumes.extend(managed_volumes.clone());
    }
    document["name"] = managed["name"].clone();
    document["networks"] = Value::Object(networks);
    document["volumes"] = Value::Object(volumes);
    let mut plan = protected(document, applications, roles)?;
    plan.connection_databases = connection_databases;
    plan.readiness_probe = spec.readiness_probe.clone();
    Ok(plan)
}
fn hex_name(name: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(name.as_bytes()))[..16].to_owned()
}

fn protected(
    mut document: Value,
    application_services: Vec<String>,
    managed_services: BTreeMap<String, String>,
) -> Result<Plan> {
    fn visit(value: &mut Value, environment: &mut BTreeMap<String, String>) {
        match value {
            Value::String(text) => {
                let name = format!("SP_VALUE_{}", environment.len());
                environment.insert(name.clone(), std::mem::take(text));
                *text = format!("${{{name}}}");
            }
            Value::Array(items) => {
                for item in items {
                    visit(item, environment)
                }
            }
            Value::Object(items) => {
                for value in items.values_mut() {
                    visit(value, environment)
                }
            }
            _ => {}
        }
    }
    let mut environment = BTreeMap::new();
    visit(&mut document, &mut environment);
    Ok(Plan {
        document,
        environment,
        application_services,
        managed_services,
        connection_databases: Default::default(),
        readiness_probe: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_plan_contains_no_literal_secret_and_preserves_dollar_values() -> Result<()> {
        let plan = protected(
            json!({"services":{"app":{"image":"example:test","environment":{"TOKEN":"secret $literal ${value}"}}}}),
            vec!["app".into()],
            BTreeMap::new(),
        )?;
        let serialized = serde_json::to_string(&plan.document)?;
        assert!(!serialized.contains("secret"));
        assert!(
            plan.environment
                .values()
                .any(|value| value == "secret $literal ${value}")
        );
        Ok(())
    }
}

#[cfg(test)]
mod integration {
    use super::*;
    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    #[ignore = "Requires Docker Compose CLI (does not start containers)"]
    fn compose_round_trip_isolates_resources_replaces_databases_and_preserves_secrets() -> Result<()>
    {
        let root = std::env::temp_dir().join(format!(
            "sandbox-plan-{}",
            sandbox_platform::docker::nonce()?
        ));
        fs::create_dir_all(&root)?;
        let _cleanup = Directory(root.clone());
        let catalog = Catalog::open(root.join("data/application.sqlite3"))?;
        let project = catalog.create_project("Fixture".into(), None, Default::default())?;
        let directory = root.join("project with spaces");
        fs::create_dir_all(directory.join("config"))?;
        let original = json!({"services":{
            "api":{"image":"alpine:3.21","profiles":["web"],"environment":{"TOKEN":"${TOKEN}","UNCHANGED":"$$literal ${TOKEN}"},"env_file":["secrets.env"],"depends_on":{"db":{"condition":"service_healthy"}},"ports":["8000:8000"],"volumes":["./storage:/data","./config:/config:ro",".:/code"],"networks":["app"]},
            "db":{"image":"mysql:8.4"},"other":{"image":"alpine:3.21","profiles":["unused"]}
        },"volumes":{"shared":{"external":true}},"networks":{"app":{"name":"shared-app"}}});
        let file = directory.join("compose.yml");
        let bytes = serde_json::to_vec_pretty(&original)?;
        fs::write(&file, &bytes)?;
        fs::write(directory.join("secrets.env"), "FROM_FILE=project-value\n")?;
        catalog.save_environment(
            &project.id,
            "*",
            &BTreeMap::from([("TOKEN".into(), "private $literal ${never_expand}".into())]),
        )?;
        let mut sandbox = catalog.create_sandbox(project.id.clone(), "First".into())?;
        sandbox.source = Some(SourceSelection {
            mode: SourceMode::CurrentCheckout,
            directory: directory.clone(),
            owned: false,
            revision: "fixture".into(),
        });
        let mut spec = ComposeLaunchSpec {
            files: vec!["compose.yml".into()],
            profiles: vec!["web".into()],
            private_external_resources: true,
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
        let managed = json!({"name":"sp-fixture-first","services":{"mysql":{"image":"percona/percona-server:8.4","labels":{scope_label("session"):"fixture"}},"clickhouse":{"image":"clickhouse/clickhouse-server:26.8","labels":{scope_label("session"):"fixture"}}},"volumes":{}});
        let cancel = AtomicBool::new(false);
        let plan = build(
            &catalog,
            &project,
            &sandbox,
            "generation-first",
            managed.clone(),
            &cancel,
        )?;
        let generated = root.join("generated.json");
        let json = serde_json::to_string_pretty(&plan.document)?;
        assert!(!json.contains("private"));
        fs::write(&generated, json)?;
        let output = Docker::discover().capture(
            &strings(&[
                "compose",
                "--env-file",
                "/dev/null",
                "-f",
                &generated.display().to_string(),
                "config",
                "--format",
                "json",
            ]),
            &plan.environment,
            &cancel,
            Duration::from_secs(60),
        )?;
        let mut resolved: Value = serde_json::from_str(&output)?;
        decode_export(&mut resolved);
        assert_eq!(
            resolved["services"]["api"]["environment"]["TOKEN"],
            "private $literal ${never_expand}"
        );
        assert_eq!(resolved["services"]["api"]["environment"]["DB_HOST"], "db");
        assert_eq!(
            resolved["services"]["db"]["image"],
            managed["services"]["mysql"]["image"]
        );
        assert!(resolved["services"].get("other").is_none());
        assert!(plan.document["services"]["api"].get("env_file").is_some());
        assert_eq!(
            resolved["services"]["api"]["environment"]["FROM_FILE"],
            "project-value"
        );
        assert_eq!(
            resolved["services"]["api"]["container_name"],
            "sp-fixture-first-app-api"
        );
        assert_eq!(resolved["services"]["api"]["ports"][0]["published"], "0");
        assert_eq!(
            resolved["services"]["api"]["ports"][0]["host_ip"],
            "127.0.0.1"
        );
        assert!(
            resolved["services"]["api"]["volumes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["target"] == "/data"
                    && v["source"]
                        .as_str()
                        .unwrap()
                        .starts_with(&sandbox.root.display().to_string()))
        );
        assert!(resolved["services"]["db"]["networks"].get("app").is_some());
        assert_eq!(fs::read(file)?, bytes);
        let second = catalog.create_sandbox(project.id.clone(), "Second".into())?;
        assert_ne!(sandbox.password, second.password);
        Ok(())
    }
}

#[cfg(test)]
mod automatic_integration {
    use super::*;
    #[test]
    #[ignore = "Requires Docker Compose CLI; inspects disposable files, starts no containers"]
    fn project_profile_connects_api_and_workers_to_each_sandbox_and_preserves_source_files()
    -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-auto-plan-{}",
            sandbox_platform::docker::nonce()?
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        fs::create_dir_all(&root)?;
        let _cleanup = Cleanup(root.clone());
        let directory = root.join("project");
        fs::create_dir_all(directory.join("src"))?;
        let git = |args: &[&str]| -> Result<()> {
            let result = std::process::Command::new("git")
                .arg("-C")
                .arg(&directory)
                .args(args)
                .output()?;
            ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            Ok(())
        };
        git(&["init", "-b", "main"])?;
        git(&["config", "user.name", "Fixture"])?;
        git(&["config", "user.email", "fixture@example.invalid"])?;
        git(&["config", "commit.gpgsign", "false"])?;
        git(&["config", "core.hooksPath", "/dev/null"])?;
        let document = json!({"services":{
            "api":{"image":"alpine:3.21","env_file":["app.env"],"environment":{"DB_HOST":"production-explicit","PROJECT_TOKEN":"${PROJECT_TOKEN}","REMOVED":null},"volumes":["./src:/code","./storage:/storage"],"networks":["app"]},
            "worker":{"image":"alpine:3.21","env_file":["app.env"],"networks":["app"]},
            "database":{"image":"mysql:8.4"},"analytics":{"image":"clickhouse/clickhouse-server:26.8"},"redis":{"image":"redis:7"}
        },"networks":{"app":{}}});
        let original = serde_json::to_vec_pretty(&document)?;
        fs::write(directory.join("compose.json"), &original)?;
        fs::write(directory.join("src/main.rs"), "fn main() {}")?;
        git(&["add", "compose.json", "src/main.rs"])?;
        git(&["commit", "-m", "fixture"])?;
        let second = root.join("second");
        git(&["worktree", "add", "-b", "second", second.to_str().unwrap()])?;
        let catalog = Catalog::open(root.join("data/application.sqlite3"))?;
        let project = catalog.create_project("Fixture".into(), None, Default::default())?;
        let env_file = "DB_NAME=app\nDB_HOST=production\nDB_PASSWORD=production-password\nCLICKHOUSE_DATABASE=analytics\nCLICKHOUSE_URL=https://production.invalid/analytics\nTOKEN='private $literal ${keep}'\nREMOVED=from-file\n";
        for working in [&directory, &second] {
            fs::write(working.join("app.env"), env_file)?;
            fs::write(working.join(".env"), "PROJECT_TOKEN=from-dotenv\n")?;
        }
        let cancel = AtomicBool::new(false);
        let mut passwords = vec![];
        for (index, working) in [directory.clone(), second.clone()].into_iter().enumerate() {
            let mut sandbox =
                catalog.create_sandbox(project.id.clone(), format!("Fixture {index}"))?;
            sandbox.source = Some(SourceSelection {
                directory: working.clone(),
                owned: false,
                revision: "fixture".into(),
                mode: SourceMode::ExistingWorktree {
                    directory: working.clone(),
                },
            });
            let spec = crate::launch_profile::prepare(
                &catalog,
                &sandbox,
                ComposeLaunchSpec {
                    files: vec!["compose.json".into()],
                    ..Default::default()
                },
                &cancel,
            )?;
            assert!(matches!(spec.mounts["api:/code"], MountRole::Source));
            assert!(matches!(spec.mounts["api:/storage"], MountRole::Data));
            assert_eq!(spec.automatic_connections.as_ref().unwrap().mysql, "app");
            sandbox.compose = Some(spec);
            let plan = build(
                &catalog,
                &project,
                &sandbox,
                &format!("generation-{index}"),
                json!({"name":format!("sp-fixture-{index}"),"services":{"mysql":{"image":"mysql:8.4","labels":{"ru.mentormarketplace.sandbox-prepare.session":"fixture"}},"clickhouse":{"image":"clickhouse/clickhouse-server:26.8"}},"volumes":{}}),
                &cancel,
            )?;
            assert!(!serde_json::to_string(&plan.document)?.contains(&sandbox.password));
            let generated = working.join("generated.json");
            fs::write(&generated, serde_json::to_vec(&plan.document)?)?;
            let resolved = inspect_environment(
                &working,
                &ComposeLaunchSpec {
                    files: vec!["generated.json".into()],
                    ..Default::default()
                },
                &plan.environment,
                &cancel,
            )?;
            for service in ["api", "worker"] {
                let env = &resolved["services"][service]["environment"];
                assert_eq!(env["DB_HOST"], "database");
                assert_eq!(env["DB_NAME"], "app");
                assert_eq!(env["DB_USER"], "sandbox");
                assert_eq!(env["DB_PASSWORD"], sandbox.password);
                assert_eq!(env["CLICKHOUSE_HOST"], "analytics");
                assert_eq!(env["CLICKHOUSE_PORT"], "8123");
                assert_eq!(env["CLICKHOUSE_URL"], "http://analytics:8123/analytics");
                assert_eq!(env["CLICKHOUSE_PASSWORD"], sandbox.password);
                assert_eq!(env["TOKEN"], "private $literal ${keep}");
                assert!(
                    resolved["services"]["database"]["networks"]
                        .get("app")
                        .is_some()
                );
            }
            assert!(
                resolved["services"]["redis"]["environment"]
                    .get("DB_PASSWORD")
                    .is_none()
            );
            assert_eq!(fs::read(working.join("compose.json"))?, original);
            assert_eq!(fs::read_to_string(working.join("app.env"))?, env_file);
            assert_eq!(
                resolved["services"]["api"]["environment"]["PROJECT_TOKEN"],
                "from-dotenv"
            );
            assert!(resolved["services"]["api"]["environment"]["REMOVED"].is_null());
            assert_eq!(
                resolved["services"]["worker"]["environment"]["REMOVED"],
                "from-file"
            );
            assert!(
                !plan
                    .environment
                    .values()
                    .any(|v| v == "private $literal ${keep}")
            );
            // Env files remain live inputs: a later Compose resolution reads updated settings.
            fs::write(
                working.join("app.env"),
                env_file.replace("private $literal ${keep}", "updated token"),
            )?;
            let fresh = inspect_environment(
                &working,
                &ComposeLaunchSpec {
                    files: vec!["generated.json".into()],
                    ..Default::default()
                },
                &plan.environment,
                &cancel,
            )?;
            assert_eq!(
                fresh["services"]["worker"]["environment"]["TOKEN"],
                "updated token"
            );
            assert_eq!(
                fresh["services"]["worker"]["environment"]["DB_PASSWORD"],
                sandbox.password
            );
            passwords.push(sandbox.password);
        }
        assert_ne!(passwords[0], passwords[1]);
        Ok(())
    }
}
