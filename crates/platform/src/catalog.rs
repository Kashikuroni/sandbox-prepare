//! Versioned application metadata. Migration never moves existing Docker bind paths.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sandbox_core::{StorageConfig, models::*};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

const VERSION: i64 = 2;
#[derive(Clone)]
pub struct Catalog {
    pub database: PathBuf,
    pub root: PathBuf,
    pub owner: String,
    connection: Arc<Mutex<Connection>>,
}

fn table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}
fn identifier<T: TryFrom<String, Error = String>>() -> Result<T> {
    T::try_from(crate::docker::nonce()?).map_err(anyhow::Error::msg)
}
impl Catalog {
    pub fn open(database: PathBuf) -> Result<Self> {
        let root = database
            .parent()
            .context("Missing application directory")?
            .to_path_buf();
        fs::create_dir_all(&root)?;
        let mut db = Connection::open(&database)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= VERSION,
            "Application database was created by a newer version"
        );
        if version < VERSION {
            let populated: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')", [], |r|r.get(0))?;
            if populated {
                let backups = root.join("migrations");
                fs::create_dir_all(&backups)?;
                let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
                let destination = backups.join(format!("before-v{}-{stamp}.sqlite3", version + 1));
                db.execute(
                    "VACUUM INTO ?1",
                    [destination.to_str().context("Invalid backup path")?],
                )?;
                private(&destination, false)?;
            }
        }
        if version == 0 {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE installation(id INTEGER PRIMARY KEY CHECK(id=1),owner TEXT NOT NULL);
                CREATE TABLE projects(id TEXT PRIMARY KEY,json TEXT NOT NULL);
                CREATE TABLE sandboxes(id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id),json TEXT NOT NULL);
                CREATE TABLE sandbox_generations(sandbox_id TEXT PRIMARY KEY REFERENCES sandboxes(id) ON DELETE CASCADE,json TEXT NOT NULL);
                CREATE TABLE project_backups(project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,manifest TEXT NOT NULL);
                CREATE TABLE project_environment(project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,service TEXT NOT NULL,key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(project_id,service,key));")?;
            let identity: Option<(String, String, String)> = if table(&tx, "sandbox_identity")? {
                tx.query_row(
                    "SELECT owner,password,root_password FROM sandbox_identity WHERE id=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
            } else {
                None
            };
            let owner = identity
                .as_ref()
                .map(|i| i.0.clone())
                .unwrap_or(crate::docker::nonce()?);
            tx.execute("INSERT INTO installation VALUES(1,?1)", [&owner])?;
            let storage = if table(&tx, "storage")? {
                tx.query_row(
                    "SELECT endpoint,bucket,region,access_key,secret_key FROM storage WHERE id=1",
                    [],
                    |r| {
                        Ok(StorageConfig {
                            endpoint: r.get(0)?,
                            bucket: r.get(1)?,
                            region: r.get(2)?,
                            access_key: r.get(3)?,
                            secret_key: r.get(4)?,
                        })
                    },
                )
                .optional()?
                .unwrap_or_default()
            } else {
                StorageConfig::default()
            };
            let project = Project {
                id: identifier()?,
                name: "Imported project".into(),
                repository: None,
                storage,
                databases: database_presets(),
                backup_root: root.join("backups"),
            };
            tx.execute(
                "INSERT INTO projects VALUES(?1,?2)",
                params![project.id.as_str(), serde_json::to_string(&project)?],
            )?;
            if table(&tx, "backup_pair")?
                && let Some(manifest) = tx
                    .query_row("SELECT manifest FROM backup_pair WHERE id=1", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?
            {
                // Validate before importing; an unreadable manifest must never trigger cleanup.
                let _: serde_json::Value =
                    serde_json::from_str(&manifest).context("Invalid legacy backup manifest")?;
                tx.execute(
                    "INSERT INTO project_backups VALUES(?1,?2)",
                    params![project.id.as_str(), manifest],
                )?;
            }
            if let Some((_, password, root_password)) = identity {
                let sandbox = Sandbox {
                    id: identifier()?,
                    project_id: project.id,
                    name: "Imported sandbox".into(),
                    root: root.join("sandboxes"),
                    password,
                    root_password,
                    source: None,
                    compose: None,
                };
                tx.execute(
                    "INSERT INTO sandboxes VALUES(?1,?2,?3)",
                    params![
                        sandbox.id.as_str(),
                        sandbox.project_id.as_str(),
                        serde_json::to_string(&sandbox)?
                    ],
                )?;
                let saved = if table(&tx, "sandbox_state")? {
                    tx.query_row("SELECT json FROM sandbox_state WHERE id=1", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?
                } else {
                    None
                };
                if let Some(saved) = saved {
                    let _: serde_json::Value =
                        serde_json::from_str(&saved).context("Invalid legacy sandbox state")?;
                    tx.execute(
                        "INSERT INTO sandbox_generations VALUES(?1,?2)",
                        params![sandbox.id.as_str(), saved],
                    )?;
                }
            }
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
        }
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 2 {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE generation_plans(sandbox_id TEXT NOT NULL REFERENCES sandboxes(id) ON DELETE CASCADE,generation TEXT NOT NULL,json TEXT NOT NULL,PRIMARY KEY(sandbox_id,generation));")?;
            tx.pragma_update(None, "user_version", 2)?;
            tx.commit()?;
        }
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        let owner = db.query_row("SELECT owner FROM installation WHERE id=1", [], |r| {
            r.get(0)
        })?;
        private(&root, true)?;
        private(&database, false)?;
        Ok(Self {
            database,
            root,
            owner,
            connection: Arc::new(Mutex::new(db)),
        })
    }
    fn db(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Application catalog lock was poisoned"))
    }
    fn all<T: DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>> {
        let db = self.db()?;
        let mut statement = db.prepare(sql)?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
    pub fn projects(&self) -> Result<Vec<Project>> {
        self.all("SELECT json FROM projects ORDER BY rowid")
    }
    pub fn sandboxes(&self) -> Result<Vec<Sandbox>> {
        self.all("SELECT json FROM sandboxes ORDER BY rowid")
    }
    pub fn project(&self, id: &ProjectId) -> Result<Project> {
        self.projects()?
            .into_iter()
            .find(|p| p.id == *id)
            .context("Project not found")
    }
    pub fn sandbox(&self, id: &SandboxId) -> Result<Sandbox> {
        self.sandboxes()?
            .into_iter()
            .find(|s| s.id == *id)
            .context("Sandbox not found")
    }
    pub fn save_project(&self, project: &Project) -> Result<()> {
        ensure!(!project.name.trim().is_empty(), "Enter a project name");
        self.db()?.execute(
            "INSERT INTO projects VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
            params![project.id.as_str(), serde_json::to_string(project)?],
        )?;
        Ok(())
    }
    pub fn create_project(
        &self,
        name: String,
        repository: Option<Repository>,
        storage: StorageConfig,
    ) -> Result<Project> {
        let id: ProjectId = identifier()?;
        let project = Project {
            id: id.clone(),
            name,
            repository,
            storage,
            databases: database_presets(),
            backup_root: self.root.join("projects").join(id.as_str()).join("backups"),
        };
        self.save_project(&project)?;
        Ok(project)
    }
    pub fn save_sandbox(&self, sandbox: &Sandbox) -> Result<()> {
        ensure!(!sandbox.name.trim().is_empty(), "Enter a sandbox name");
        self.db()?.execute("INSERT INTO sandboxes VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET json=excluded.json",params![sandbox.id.as_str(),sandbox.project_id.as_str(),serde_json::to_string(sandbox)?])?;
        Ok(())
    }
    pub fn create_sandbox(&self, project_id: ProjectId, name: String) -> Result<Sandbox> {
        self.project(&project_id)?;
        let id: SandboxId = identifier()?;
        let sandbox = Sandbox {
            id: id.clone(),
            project_id,
            name,
            root: self.root.join("sandboxes").join(id.as_str()),
            password: crate::docker::nonce()?,
            root_password: crate::docker::nonce()?,
            source: None,
            compose: None,
        };
        self.save_sandbox(&sandbox)?;
        Ok(sandbox)
    }
    pub fn remove_sandbox(&self, id: &SandboxId) -> Result<()> {
        self.db()?
            .execute("DELETE FROM sandboxes WHERE id=?1", [id.as_str()])?;
        Ok(())
    }
    pub fn sandbox_state<T: DeserializeOwned + Default>(&self, id: &SandboxId) -> Result<T> {
        self.read_json(
            "SELECT json FROM sandbox_generations WHERE sandbox_id=?1",
            id.as_str(),
        )
        .map(|v| v.unwrap_or_default())
    }
    pub fn save_sandbox_state(&self, id: &SandboxId, value: &impl Serialize) -> Result<()> {
        self.db()?.execute("INSERT INTO sandbox_generations VALUES(?1,?2) ON CONFLICT(sandbox_id) DO UPDATE SET json=excluded.json",params![id.as_str(),serde_json::to_string(value)?])?;
        Ok(())
    }
    fn read_json<T: DeserializeOwned>(&self, sql: &str, id: &str) -> Result<Option<T>> {
        let json: Option<String> = self.db()?.query_row(sql, [id], |r| r.get(0)).optional()?;
        json.map(|json| Ok(serde_json::from_str(&json)?))
            .transpose()
    }
    pub fn backup_manifest(&self, id: &ProjectId) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row(
                "SELECT manifest FROM project_backups WHERE project_id=?1",
                [id.as_str()],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn save_backup_manifest(&self, id: &ProjectId, manifest: &str) -> Result<()> {
        self.db()?.execute("INSERT INTO project_backups VALUES(?1,?2) ON CONFLICT(project_id) DO UPDATE SET manifest=excluded.manifest",params![id.as_str(),manifest])?;
        Ok(())
    }
    pub fn generation_plan<T: DeserializeOwned>(
        &self,
        id: &SandboxId,
        generation: &str,
    ) -> Result<Option<T>> {
        let json: Option<String> = self
            .db()?
            .query_row(
                "SELECT json FROM generation_plans WHERE sandbox_id=?1 AND generation=?2",
                params![id.as_str(), generation],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }
    pub fn save_generation_plan(
        &self,
        id: &SandboxId,
        generation: &str,
        value: &impl Serialize,
    ) -> Result<()> {
        self.db()?.execute("INSERT INTO generation_plans VALUES(?1,?2,?3) ON CONFLICT(sandbox_id,generation) DO UPDATE SET json=excluded.json",params![id.as_str(),generation,serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn remove_generation_plan(&self, id: &SandboxId, generation: &str) -> Result<()> {
        self.db()?.execute(
            "DELETE FROM generation_plans WHERE sandbox_id=?1 AND generation=?2",
            params![id.as_str(), generation],
        )?;
        Ok(())
    }
    pub fn environment(
        &self,
        id: &ProjectId,
        service: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let db = self.db()?;
        let mut statement = db.prepare(
            "SELECT key,value FROM project_environment WHERE project_id=?1 AND service=?2",
        )?;
        Ok(statement
            .query_map(params![id.as_str(), service], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn save_environment(
        &self,
        id: &ProjectId,
        service: &str,
        values: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction()?;
        tx.execute(
            "DELETE FROM project_environment WHERE project_id=?1 AND service=?2",
            params![id.as_str(), service],
        )?;
        for (key, value) in values {
            tx.execute(
                "INSERT INTO project_environment VALUES(?1,?2,?3,?4)",
                params![id.as_str(), service, key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}
fn private(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_metadata_writers_share_one_connection() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-catalog-concurrent-{}",
            crate::docker::nonce()?
        ));
        let catalog = Catalog::open(root.join("application.sqlite3"))?;
        let project = catalog.projects()?.remove(0);
        std::thread::scope(|scope| -> Result<()> {
            let jobs: Vec<_> = (0..4)
                .map(|worker| {
                    let catalog = catalog.clone();
                    let project = project.id.clone();
                    scope.spawn(move || -> Result<()> {
                        for index in 0..10 {
                            let sandbox = catalog
                                .create_sandbox(project.clone(), format!("{worker}-{index}"))?;
                            catalog.save_sandbox_state(
                                &sandbox.id,
                                &serde_json::json!({"worker":worker}),
                            )?;
                            let state: serde_json::Value = catalog.sandbox_state(&sandbox.id)?;
                            assert_eq!(state["worker"], worker);
                        }
                        Ok(())
                    })
                })
                .collect();
            for job in jobs {
                job.join().unwrap()?;
            }
            Ok(())
        })?;
        assert_eq!(catalog.sandboxes()?.len(), 40);
        drop(catalog);
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn legacy_migration_preserves_paths_identity_credentials_and_generations() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("sandbox-migration-{}", crate::docker::nonce()?));
        fs::create_dir_all(&root)?;
        let path = root.join("application.sqlite3");
        let db = Connection::open(&path)?;
        db.execute_batch("CREATE TABLE sandbox_identity(id INTEGER PRIMARY KEY,owner TEXT,password TEXT,root_password TEXT); CREATE TABLE sandbox_state(id INTEGER PRIMARY KEY,json TEXT); CREATE TABLE backup_pair(id INTEGER PRIMARY KEY,manifest TEXT);")?;
        let owner = crate::docker::nonce()?;
        db.execute(
            "INSERT INTO sandbox_identity VALUES(1,?1,'password','root-password')",
            [&owner],
        )?;
        db.execute(
            "INSERT INTO sandbox_state VALUES(1,?1)",
            [r#"{"active":{"id":"existing"},"pending":null,"retired":[]}"#],
        )?;
        db.execute(
            "INSERT INTO backup_pair VALUES(1,?1)",
            [r#"{"generation":"pair-existing"}"#],
        )?;
        fs::create_dir_all(root.join("backups/pair-existing"))?;
        fs::write(root.join("backups/pair-existing/archive"), b"preserved")?;
        drop(db);
        let catalog = Catalog::open(path.clone())?;
        assert_eq!(catalog.owner, owner);
        let project = catalog.projects()?.remove(0);
        let sandbox = catalog.sandboxes()?.remove(0);
        assert_eq!(sandbox.root, root.join("sandboxes"));
        assert_eq!(sandbox.password, "password");
        assert_eq!(project.backup_root, root.join("backups"));
        assert_eq!(
            fs::read(root.join("backups/pair-existing/archive"))?,
            b"preserved"
        );
        let state: serde_json::Value = catalog.sandbox_state(&sandbox.id)?;
        assert_eq!(state["active"]["id"], "existing");
        assert!(root.join("migrations").read_dir()?.next().is_some());
        let reopened = Catalog::open(path)?;
        assert_eq!(reopened.projects()?.len(), 1);
        assert_eq!(reopened.sandboxes()?.len(), 1);
        let other = reopened.create_sandbox(project.id, "Other".into())?;
        assert_ne!(other.password, sandbox.password);
        assert_ne!(other.root, sandbox.root);
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn failed_migration_rolls_back_and_newer_schema_is_rejected() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sandbox-migration-failure-{}",
            crate::docker::nonce()?
        ));
        fs::create_dir_all(&root)?;
        let path = root.join("application.sqlite3");
        let db = Connection::open(&path)?;
        db.execute_batch("CREATE TABLE backup_pair(id INTEGER PRIMARY KEY,manifest TEXT); INSERT INTO backup_pair VALUES(1,'invalid JSON');")?;
        drop(db);
        assert!(Catalog::open(path.clone()).is_err());
        let db = Connection::open(&path)?;
        assert!(!table(&db, "installation")?);
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
            0
        );
        db.pragma_update(None, "user_version", VERSION + 1)?;
        drop(db);
        assert!(Catalog::open(path).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
