//! Persistent sandbox identity and generations; credentials stay out of Compose and env.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sandbox_core::models::{ProjectId, Sandbox, SandboxId};
use sandbox_platform::catalog::Catalog;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub project: String,
    pub ports: [u16; 3],
    #[serde(default)]
    pub dynamic_ports: bool,
    pub backups: [String; 2],
    #[serde(default)]
    pub backup_set: Option<String>,
    pub created: String,
    pub tables: [u64; 2],
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Saved {
    pub active: Option<Instance>,
    pub pending: Option<Instance>,
    pub retired: Vec<Instance>,
}
#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
    pub database: PathBuf,
    pub owner: String,
    pub password: String,
    pub root_password: String,
    pub scope: Option<(Catalog, SandboxId, ProjectId)>,
}
impl Store {
    pub fn scoped(catalog: Catalog, sandbox: Sandbox) -> Result<Self> {
        let expected = catalog.root.join("sandboxes");
        ensure!(
            sandbox.root == expected || sandbox.root == expected.join(sandbox.id.as_str()),
            "Invalid sandbox data directory"
        );
        ensure!(
            valid_id(&catalog.owner)
                && valid_id(&sandbox.password)
                && valid_id(&sandbox.root_password),
            "Invalid sandbox identity or credentials"
        );
        fs::create_dir_all(&sandbox.root)?;
        crate::logging::register_credentials(&sandbox.password, &sandbox.root_password);
        Ok(Self {
            root: sandbox.root,
            database: catalog.database.clone(),
            owner: catalog.owner.clone(),
            password: sandbox.password,
            root_password: sandbox.root_password,
            scope: Some((catalog, sandbox.id, sandbox.project_id)),
        })
    }
    pub fn cache(&self) -> Result<crate::backup_cache::Cache> {
        if let Some((catalog, _, project)) = &self.scope {
            crate::backup_cache::Cache::for_project(catalog.clone(), project.clone())
        } else {
            crate::backup_cache::Cache::for_database(self.database.clone())
        }
    }
    pub fn open(database: PathBuf) -> Result<Self> {
        let parent = database.parent().context("Invalid application data path")?;
        fs::create_dir_all(parent)?;
        let root = parent.join("sandboxes");
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let db = Connection::open(&database)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS sandbox_identity(id INTEGER PRIMARY KEY CHECK(id=1), owner TEXT NOT NULL, password TEXT NOT NULL, root_password TEXT NOT NULL); CREATE TABLE IF NOT EXISTS sandbox_state(id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL);")?;
        let identity = db
            .query_row(
                "SELECT owner,password,root_password FROM sandbox_identity WHERE id=1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let (owner, password, root_password) = if let Some(identity) = identity {
            identity
        } else {
            let values = (
                crate::docker::nonce()?,
                crate::docker::nonce()?,
                crate::docker::nonce()?,
            );
            db.execute(
                "INSERT INTO sandbox_identity VALUES(1,?1,?2,?3)",
                params![values.0, values.1, values.2],
            )?;
            values
        };
        ensure!(valid_id(&owner), "Invalid sandbox owner identity");
        ensure!(
            valid_id(&password) && valid_id(&root_password),
            "Invalid sandbox credentials"
        );
        crate::logging::register_credentials(&password, &root_password);
        Ok(Self {
            root,
            database,
            owner,
            password,
            root_password,
            scope: None,
        })
    }
    fn db(&self) -> Result<Connection> {
        let db = Connection::open(&self.database)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA synchronous=FULL;")?;
        Ok(db)
    }
    pub fn load(&self) -> Result<Saved> {
        if let Some((catalog, id, _)) = &self.scope {
            let saved: Saved = catalog.sandbox_state(id)?;
            for instance in saved
                .active
                .iter()
                .chain(saved.pending.iter())
                .chain(saved.retired.iter())
            {
                self.validate(instance)?;
            }
            return Ok(saved);
        }
        let json: Option<String> = self
            .db()?
            .query_row("SELECT json FROM sandbox_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .optional()?;
        let saved: Saved = json
            .map(|j| serde_json::from_str(&j))
            .transpose()?
            .unwrap_or_default();
        for instance in saved
            .active
            .iter()
            .chain(saved.pending.iter())
            .chain(saved.retired.iter())
        {
            self.validate(instance)?;
        }
        Ok(saved)
    }
    pub fn save(&self, saved: &Saved) -> Result<()> {
        if let Some((catalog, id, _)) = &self.scope {
            return catalog.save_sandbox_state(id, saved);
        }
        self.db()?.execute("INSERT INTO sandbox_state VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET json=excluded.json",params![serde_json::to_string(saved)?])?;
        Ok(())
    }
    pub fn validate(&self, instance: &Instance) -> Result<()> {
        ensure!(
            valid_id(&instance.id)
                && instance.project == format!("sp-{}-{}", &self.owner[..12], &instance.id[..12]),
            "Invalid sandbox identity"
        );
        Ok(())
    }
    pub fn directory(&self, instance: &Instance) -> Result<PathBuf> {
        self.validate(instance)?;
        Ok(self.root.join(&instance.id))
    }
    pub fn compose(&self, instance: &Instance) -> Result<PathBuf> {
        Ok(self.directory(instance)?.join("compose.json"))
    }
    pub fn container(&self, instance: &Instance, service: &str) -> String {
        format!("{}-{service}", instance.project)
    }
    pub fn volume(&self, instance: &Instance, index: usize) -> String {
        self.container(
            instance,
            if index == 0 {
                "mysql-data"
            } else {
                "clickhouse-data"
            },
        )
    }
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
pub fn service_file(directory: &Path, name: &str, body: &str) -> Result<()> {
    crate::docker::private_file(&directory.join(name), body.as_bytes())
}
