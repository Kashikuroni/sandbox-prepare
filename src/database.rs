use crate::backups::StorageConfig;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

pub fn database_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("Home directory is unavailable")?;
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Application Support")
    } else if cfg!(target_os = "windows") {
        PathBuf::from(
            std::env::var_os("APPDATA").context("Application data directory is unavailable")?,
        )
    } else {
        PathBuf::from(home).join(".local/share")
    };
    Ok(base.join("sandbox-prepare/application.sqlite3"))
}

fn open(path: &Path) -> Result<Connection> {
    let parent = path.parent().context("Invalid database path")?;
    std::fs::create_dir_all(parent)?;
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(3))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS storage (id INTEGER PRIMARY KEY CHECK(id=1), endpoint TEXT NOT NULL, bucket TEXT NOT NULL, region TEXT NOT NULL, access_key TEXT NOT NULL, secret_key TEXT NOT NULL);")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(db)
}

pub fn load_storage() -> Result<Option<StorageConfig>> {
    let result = (|| read(&open(&database_path()?)?))();
    if let Ok(Some(config)) = &result {
        crate::logging::register_credentials(&config.access_key, &config.secret_key);
    }
    match &result {
        Ok(config) => {
            tracing::debug!(target:"app::storage",configured=config.is_some(),"Storage settings loaded")
        }
        Err(error) => {
            tracing::error!(target:"app::storage",%error,"Could not load storage settings")
        }
    }
    result
}
pub fn save_storage(config: &StorageConfig) -> Result<()> {
    crate::logging::register_credentials(&config.access_key, &config.secret_key);
    let result = (|| write(&open(&database_path()?)?, config))();
    match &result {
        Ok(()) => tracing::info!(target:"app::storage","Storage connection saved"),
        Err(error) => {
            tracing::error!(target:"app::storage",%error,"Could not save storage settings")
        }
    }
    result
}
fn read(db: &Connection) -> Result<Option<StorageConfig>> {
    Ok(db
        .query_row(
            "SELECT endpoint,bucket,region,access_key,secret_key FROM storage WHERE id=1",
            [],
            |row| {
                Ok(StorageConfig {
                    endpoint: row.get(0)?,
                    bucket: row.get(1)?,
                    region: row.get(2)?,
                    access_key: row.get(3)?,
                    secret_key: row.get(4)?,
                })
            },
        )
        .optional()?)
}
fn write(db: &Connection, config: &StorageConfig) -> Result<()> {
    db.execute("INSERT INTO storage(id,endpoint,bucket,region,access_key,secret_key) VALUES(1,?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET endpoint=excluded.endpoint,bucket=excluded.bucket,region=excluded.region,access_key=excluded.access_key,secret_key=excluded.secret_key",params![config.endpoint,config.bucket,config.region,config.access_key,config.secret_key])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_survive_reopening_and_can_be_replaced() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("sandbox-database-{}.sqlite", std::process::id()));
        let config = StorageConfig {
            access_key: "test'access".into(),
            secret_key: "test;secret".into(),
            ..Default::default()
        };
        {
            let db = open(&path)?;
            assert!(read(&db)?.is_none());
            write(&db, &config)?;
        }
        {
            let db = open(&path)?;
            let loaded = read(&db)?.unwrap();
            assert_eq!(loaded.secret_key, config.secret_key);
            assert_eq!(loaded.access_key, config.access_key);
            write(&db, &StorageConfig::default())?;
        }
        assert!(read(&open(&path)?)?.unwrap().secret_key.is_empty());
        std::fs::remove_file(path)?;
        Ok(())
    }
}
