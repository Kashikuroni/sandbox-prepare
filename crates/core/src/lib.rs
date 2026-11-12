use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod models;
pub mod operations;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::System => "System",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct LogSettings {
    pub max_file_mib: u32,
    pub retention_days: u32,
    pub check_interval_hours: u32,
}
impl Default for LogSettings {
    fn default() -> Self {
        Self {
            max_file_mib: 10,
            retention_days: 7,
            check_interval_hours: 24,
        }
    }
}
impl LogSettings {
    pub fn validate(self) -> Result<()> {
        anyhow::ensure!(
            (1..=10240).contains(&self.max_file_mib),
            "File size must be between 1 and 10240 MiB"
        );
        anyhow::ensure!(
            (1..=36500).contains(&self.retention_days),
            "Retention must be between 1 and 36500 days"
        );
        anyhow::ensure!(
            (1..=87600).contains(&self.check_interval_hours),
            "Check interval must be between 1 and 87600 hours"
        );
        Ok(())
    }
    pub fn max_bytes(self) -> u64 {
        u64::from(self.max_file_mib) * 1024 * 1024
    }
    pub fn retention(self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.retention_days) * 86400)
    }
    pub fn interval_seconds(self) -> u64 {
        u64::from(self.check_interval_hours) * 3600
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StorageConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://services.mentormarketplace.ru:9443".into(),
            bucket: "ms-bucket".into(),
            region: "us-east-1".into(),
            access_key: String::new(),
            secret_key: String::new(),
        }
    }
}
