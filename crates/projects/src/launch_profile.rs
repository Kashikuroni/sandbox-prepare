//! Project defaults and the standard DB_* / CLICKHOUSE_* runtime contract.
use anyhow::{Context, Result, ensure};
use sandbox_core::models::*;
use sandbox_platform::catalog::Catalog;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::atomic::AtomicBool,
};

pub fn resolve_paths(mut spec: ComposeLaunchSpec, directory: &Path) -> ComposeLaunchSpec {
    for path in spec.mount_paths.values_mut() {
        if path.is_relative() {
            *path = directory.join(&*path);
        }
    }
    spec
}
pub fn portable(mut spec: ComposeLaunchSpec, directory: &Path) -> ComposeLaunchSpec {
    for path in spec.mount_paths.values_mut() {
        if let Ok(relative) = path.strip_prefix(directory) {
            *path = relative.to_path_buf();
        }
    }
    spec
}
pub fn environment(
    names: &DatabaseNames,
    mysql: &str,
    clickhouse: &str,
    password: &str,
) -> BTreeMap<String, String> {
    [
        ("DB_NAME", names.mysql.as_str()),
        ("DB_USER", "sandbox"),
        ("DB_PASSWORD", password),
        ("DB_HOST", mysql),
        ("DB_PORT", "3306"),
        ("CLICKHOUSE_HOST", clickhouse),
        ("CLICKHOUSE_PORT", "8123"),
        ("CLICKHOUSE_DATABASE", names.clickhouse.as_str()),
        ("CLICKHOUSE_LOGIN", "sandbox"),
        ("CLICKHOUSE_PASSWORD", password),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .chain([(
        "CLICKHOUSE_URL".into(),
        format!("http://{clickhouse}:8123/{}", names.clickhouse),
    )])
    .collect()
}
pub fn uses_mysql(env: &BTreeMap<String, String>) -> bool {
    env.keys()
        .any(|k| ["DB_NAME", "DB_USER", "DB_PASSWORD", "DB_HOST", "DB_PORT"].contains(&k.as_str()))
}
pub fn uses_clickhouse(env: &BTreeMap<String, String>) -> bool {
    env.keys().any(|k| k.starts_with("CLICKHOUSE_"))
}
/// Only DB consumers receive connections; workers sharing the same env receive the same contract.
pub fn inject(
    env: &mut BTreeMap<String, String>,
    values: &BTreeMap<String, String>,
) -> (bool, bool) {
    let mysql = uses_mysql(env);
    let clickhouse = uses_clickhouse(env);
    for (key, value) in values {
        if mysql && key.starts_with("DB_") || clickhouse && key.starts_with("CLICKHOUSE_") {
            env.insert(key.clone(), value.clone());
        }
    }
    (mysql, clickhouse)
}
pub fn infer_roles(document: &Value, spec: &mut ComposeLaunchSpec) -> Result<()> {
    for role in ["mysql", "clickhouse"] {
        let id = DatabaseId::try_from(role.to_owned()).unwrap();
        let configured = spec
            .databases
            .iter()
            .find(|(_, value)| **value == id)
            .map(|(name, _)| name.clone());
        if configured.as_deref().is_some_and(|name| name != role) {
            continue;
        }
        let candidates: Vec<_> = document["services"]
            .as_object()
            .into_iter()
            .flat_map(|s| s.iter())
            .filter(|(_, service)| {
                let image = service["image"].as_str().unwrap_or("").to_ascii_lowercase();
                if role == "mysql" {
                    image.contains("mysql")
                        || image.contains("percona-server")
                        || image.contains("mariadb")
                } else {
                    image.contains("clickhouse-server")
                }
            })
            .map(|(name, _)| name.clone())
            .collect();
        ensure!(
            candidates.len() <= 1,
            "Multiple {role} services: choose the service to replace in Advanced settings"
        );
        spec.databases.retain(|_, value| *value != id);
        spec.databases.insert(
            candidates.into_iter().next().unwrap_or_else(|| role.into()),
            id,
        );
    }
    Ok(())
}
fn concrete(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains("${")
}
fn url_database(value: &str) -> Option<String> {
    let (_, rest) = value.split_once("://")?;
    let (_, path) = rest.split_once('/')?;
    let name = path.split(['?', '#']).next()?.trim_matches('/');
    concrete(name).then(|| name.to_owned())
}
pub fn infer_mounts(
    directory: &Path,
    document: &Value,
    cancel: &AtomicBool,
) -> Result<BTreeMap<String, MountRole>> {
    let tracked =
        crate::git::Git::default().run(directory, &["ls-files".into(), "-z".into()], cancel)?;
    let tracked: Vec<_> = tracked
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n == ".gitignore" || n == ".gitkeep")
        })
        .collect();
    let mut roles = BTreeMap::new();
    for (name, service) in document["services"]
        .as_object()
        .into_iter()
        .flat_map(|s| s.iter())
    {
        for mount in service["volumes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["type"] == "bind")
        {
            let path = Path::new(mount["source"].as_str().unwrap_or(""));
            let source = path == directory
                || path
                    .strip_prefix(directory)
                    .is_ok_and(|relative| tracked.iter().any(|p| p.starts_with(relative)));
            let role = if mount["read_only"] == true {
                MountRole::Configuration
            } else if source {
                MountRole::Source
            } else {
                MountRole::Data
            };
            if let Some(target) = mount["target"].as_str() {
                roles.insert(format!("{name}:{target}"), role);
            }
        }
    }
    Ok(roles)
}
pub fn prepare(
    catalog: &Catalog,
    sandbox: &Sandbox,
    mut spec: ComposeLaunchSpec,
    cancel: &AtomicBool,
) -> Result<ComposeLaunchSpec> {
    let source = sandbox
        .source
        .as_ref()
        .context("Choose a working copy first")?;
    ensure!(
        source.directory.is_dir(),
        "The selected working copy is unavailable"
    );
    crate::local_environment::prepare(
        catalog,
        &catalog.project(&sandbox.project_id)?,
        sandbox,
        cancel,
    )?;
    if let Some(step) = &spec.preparation {
        crate::preparation::validate(sandbox, step)?;
    }
    spec = resolve_paths(spec, &source.directory);
    let global = catalog.environment(&sandbox.project_id, "*")?;
    let document = crate::compose::inspect(&source.directory, &spec, &global, cancel, false)?;
    infer_roles(&document, &mut spec)?;
    for (key, role) in infer_mounts(&source.directory, &document, cancel)? {
        spec.mounts.entry(key).or_insert(role);
    }

    let included = crate::compose::included_services(
        document["services"]
            .as_object()
            .context("Compose has no services")?,
        &spec,
        &spec.databases.keys().cloned().collect(),
    )?;
    let resolved = crate::compose::inspect_environment(&source.directory, &spec, &global, cancel)?;
    let mut names = spec.automatic_connections.clone().unwrap_or_default();
    let mut mysql = BTreeSet::new();
    let mut clickhouse = BTreeSet::new();
    let mut needs_mysql = false;
    let mut needs_clickhouse = false;
    for (service, config) in document["services"]
        .as_object()
        .context("Compose has no services")?
    {
        if !included.contains(service) || spec.databases.contains_key(service) {
            continue;
        }
        let scoped = catalog.environment(&sandbox.project_id, service)?;
        let mut env = crate::compose::service_environment(&resolved, service);
        if config.get("env_file").is_some() {
            env.extend(global.clone());
        }
        env.extend(scoped);
        if let Some(explicit) = config["environment"].as_object() {
            for (key, value) in explicit {
                if let Some(value) = value.as_str().filter(|v| concrete(v)) {
                    env.insert(key.clone(), value.into());
                } else {
                    env.entry(key.clone()).or_default();
                }
            }
        }
        if let Some(value) = env.get("DB_NAME").filter(|v| concrete(v)) {
            mysql.insert(value.clone());
        }
        if let Some(value) = env.get("CLICKHOUSE_DATABASE").filter(|v| concrete(v)) {
            clickhouse.insert(value.clone());
        } else if let Some(name) = env.get("CLICKHOUSE_URL").and_then(|v| url_database(v)) {
            clickhouse.insert(name);
        }
        needs_mysql |= uses_mysql(&env);
        needs_clickhouse |= uses_clickhouse(&env);
    }
    ensure!(
        !needs_mysql || !names.mysql.trim().is_empty() || !mysql.is_empty(),
        "Set the MySQL database name in the project profile (DB_NAME)"
    );
    ensure!(
        !needs_clickhouse || !names.clickhouse.trim().is_empty() || !clickhouse.is_empty(),
        "Set the ClickHouse database name in the project profile (CLICKHOUSE_DATABASE)"
    );
    for (name, options, role) in [
        (&mut names.mysql, mysql, "MySQL"),
        (&mut names.clickhouse, clickhouse, "ClickHouse"),
    ] {
        if name.trim().is_empty() {
            ensure!(
                options.len() <= 1,
                "Multiple {role} database names: choose a project default or configure explicit bindings"
            );
            *name = options.into_iter().next().unwrap_or_default();
        }
    }
    spec.automatic_connections = Some(names);
    if let Some(probe) = &spec.readiness_probe {
        ensure!(
            included.contains(&probe.service) && !spec.databases.contains_key(&probe.service),
            "Readiness probe must refer to an application service"
        );
        ensure!(
            probe.port > 0 && probe.path.starts_with('/') && !probe.path.contains(['\r', '\n']),
            "Enter the HTTP container port and a readiness path beginning with /"
        );
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn automatic_contract_overrides_production_values_and_does_not_touch_other_services() {
        let names = DatabaseNames {
            mysql: "app".into(),
            clickhouse: "analytics".into(),
        };
        let values = environment(&names, "db", "ch", "per-sandbox-password");
        let mut consumer = BTreeMap::from([
            ("DB_HOST".into(), "production".into()),
            ("DB_NAME".into(), "old".into()),
            ("CLICKHOUSE_URL".into(), "https://production/old".into()),
            ("TOKEN".into(), "keep".into()),
        ]);
        assert_eq!(inject(&mut consumer, &values), (true, true));
        assert_eq!(consumer["DB_HOST"], "db");
        assert_eq!(consumer["DB_NAME"], "app");
        assert_eq!(consumer["DB_PORT"], "3306");
        assert_eq!(consumer["CLICKHOUSE_URL"], "http://ch:8123/analytics");
        assert_eq!(consumer["CLICKHOUSE_LOGIN"], "sandbox");
        assert_eq!(consumer["CLICKHOUSE_PASSWORD"], "per-sandbox-password");
        assert_eq!(consumer["TOKEN"], "keep");
        let mut redis = BTreeMap::from([("REDIS_PORT".into(), "6379".into())]);
        let original = redis.clone();
        assert_eq!(inject(&mut redis, &values), (false, false));
        assert_eq!(redis, original);
        let mut mysql = BTreeMap::from([("DB_HOST".into(), "old".into())]);
        assert_eq!(inject(&mut mysql, &values), (true, false));
        assert!(!mysql.contains_key("CLICKHOUSE_HOST"));
    }
    #[test]
    fn roles_are_inferred_only_when_unambiguous_and_explicit_choices_are_preserved() -> Result<()> {
        let mut spec = ComposeLaunchSpec::default();
        infer_roles(
            &json!({"services":{"database":{"image":"percona/percona-server:8.4"},"analytics":{"image":"clickhouse/clickhouse-server:26.8"}}}),
            &mut spec,
        )?;
        assert_eq!(spec.databases["database"].as_str(), "mysql");
        assert_eq!(spec.databases["analytics"].as_str(), "clickhouse");
        let mut ambiguous = ComposeLaunchSpec::default();
        assert!(
            infer_roles(
                &json!({"services":{"a":{"image":"mysql:8"},"b":{"image":"mysql:8"}}}),
                &mut ambiguous
            )
            .is_err()
        );
        infer_roles(
            &json!({"services":{"a":{"image":"mysql:8"},"b":{"image":"mysql:8"}}}),
            &mut spec,
        )?;
        assert!(spec.databases.contains_key("database"));
        Ok(())
    }
    #[test]
    fn inherited_mount_paths_follow_the_new_working_copy() {
        let mut spec = ComposeLaunchSpec::default();
        spec.mount_paths
            .insert("api:/code".into(), "/old/worktree/backend".into());
        spec.mount_paths
            .insert("api:/tls".into(), "/shared/certificate".into());
        let inherited = resolve_paths(
            portable(spec, Path::new("/old/worktree")),
            Path::new("/new/worktree"),
        );
        assert_eq!(
            inherited.mount_paths["api:/code"],
            Path::new("/new/worktree/backend")
        );
        assert_eq!(
            inherited.mount_paths["api:/tls"],
            Path::new("/shared/certificate")
        );
    }
}
