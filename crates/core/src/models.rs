use crate::StorageConfig;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, path::PathBuf};

macro_rules! identifier {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                if value.is_empty() || value.len() > 128 || !value.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')) {
                    return Err(format!("Invalid {}", stringify!($name)));
                }
                Ok(Self(value))
            }
        }
        impl From<$name> for String { fn from(value: $name) -> Self { value.0 } }
        impl $name { pub fn as_str(&self) -> &str { &self.0 } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
    )+};
}
identifier!(
    ProjectId,
    SandboxId,
    GenerationId,
    BackupSetId,
    DatabaseId,
    OperationId
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DatabaseEngine {
    MySql,
    ClickHouse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatabaseSpec {
    pub engine: DatabaseEngine,
    pub version: String,
    pub backup_format: String,
    pub prefix: String,
}

pub fn database_presets() -> BTreeMap<DatabaseId, DatabaseSpec> {
    [
        (
            "mysql",
            DatabaseEngine::MySql,
            "8.4.11-11",
            "xbstream.zst",
            "mysql/",
        ),
        (
            "clickhouse",
            DatabaseEngine::ClickHouse,
            "26.8.19.9",
            "tar.zst",
            "ch/",
        ),
    ]
    .into_iter()
    .map(|(id, engine, version, format, prefix)| {
        (
            DatabaseId::try_from(id.to_owned()).expect("Static database ID"),
            DatabaseSpec {
                engine,
                version: version.into(),
                backup_format: format.into(),
                prefix: prefix.into(),
            },
        )
    })
    .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    pub directory: PathBuf,
    pub owned: bool,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SourceMode {
    ManagedWorktree { branch: String },
    CurrentCheckout,
    ExistingWorktree { directory: PathBuf },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceSelection {
    pub mode: SourceMode,
    pub directory: PathBuf,
    pub owned: bool,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DatabaseParameter {
    Host,
    Port,
    User,
    Password,
    Database,
    Url,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DatabaseBinding {
    pub service: String,
    pub variable: String,
    pub database: DatabaseId,
    pub parameter: DatabaseParameter,
    #[serde(default)]
    pub database_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MountRole {
    Source,
    Configuration,
    Data,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ComposeLaunchSpec {
    pub files: Vec<PathBuf>,
    pub profiles: Vec<String>,
    pub services: Vec<String>,
    /// Logical Compose database service -> configured database ID.
    pub databases: BTreeMap<String, DatabaseId>,
    pub bindings: Vec<DatabaseBinding>,
    pub mounts: BTreeMap<String, MountRole>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub repository: Option<Repository>,
    pub storage: StorageConfig,
    pub databases: BTreeMap<DatabaseId, DatabaseSpec>,
    pub backup_root: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub id: SandboxId,
    pub project_id: ProjectId,
    pub name: String,
    pub root: PathBuf,
    pub password: String,
    pub root_password: String,
    pub source: Option<SourceSelection>,
    pub compose: Option<ComposeLaunchSpec>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scope {
    pub project: Option<ProjectId>,
    pub sandbox: Option<SandboxId>,
    pub operation: Option<OperationId>,
}
