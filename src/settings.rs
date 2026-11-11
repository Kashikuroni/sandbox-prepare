use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::theme::ThemeMode;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub theme: ThemeMode,
    pub current_log_limit: usize,
    pub logging: LogSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            current_log_limit: 2000,
            logging: LogSettings::default(),
        }
    }
}

impl Settings {
    pub fn load() -> Result<Self> {
        Self::load_from(&settings_path()?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&settings_path()?)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.current_log_limit > 0,
            "Current log records must be greater than zero"
        );
        self.logging.validate()
    }

    pub fn from_json(contents: &str) -> Result<Self> {
        let settings: Self = serde_json::from_str(contents).context("Invalid settings JSON")?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn json(&self) -> Result<String> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }

    fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::from_json(&contents),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let legacy = path.with_extension("toml");
                match fs::read_to_string(&legacy) {
                    Ok(contents) => {
                        let settings: Self =
                            toml::from_str(&contents).context("Invalid legacy settings")?;
                        settings.validate()?;
                        settings.save_to(path)?;
                        Ok(settings)
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
                    Err(error) => Err(error).context("Could not read legacy settings"),
                }
            }
            Err(error) => Err(error).context("Could not read settings"),
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let contents = self.json()?;
        let parent = path.parent().context("Settings directory is missing")?;
        fs::create_dir_all(parent).context("Could not create settings directory")?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, contents).context("Could not write settings")?;
        fs::rename(&temporary, path).context("Could not replace settings file")
    }
}

fn settings_path() -> Result<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(directory) = env::var_os("SANDBOX_PREPARE_TEST_DATA_DIR") {
        return Ok(PathBuf::from(directory).join("settings.json"));
    }
    let config_dir = if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(path)
    } else if cfg!(target_os = "windows") {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("APPDATA is not set"))?
    } else {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set"))?;
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            home.join(".config")
        }
    };
    Ok(config_dir.join("sandbox-prepare/settings.json"))
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
    fn validate(self) -> Result<()> {
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

pub struct SettingsModel {
    pub value: Settings,
    pub error: Option<String>,
}
impl SettingsModel {
    pub fn load() -> Self {
        match Settings::load() {
            Ok(value) => Self { value, error: None },
            Err(error) => Self {
                value: Settings::default(),
                error: Some(format!("Could not load settings: {error:#}")),
            },
        }
    }
    pub fn save(&mut self, value: Settings, cx: &mut gpui::Context<Self>) -> Result<()> {
        value.save()?;
        let configured = crate::logging::store().configure(value.logging);
        crate::logging::store().set_limit(value.current_log_limit);
        self.value = value;
        self.error = None;
        tracing::info!(target: "app::settings", "Settings saved");
        cx.notify();
        configured.context("Settings saved, but the log writer could not apply them")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory(label: &str) -> PathBuf {
        env::temp_dir().join(format!("sandbox-settings-{label}-{}", std::process::id()))
    }
    #[test]
    fn legacy_migration_preserves_values_and_prefers_json() -> Result<()> {
        let dir = directory("migration");
        fs::create_dir_all(&dir)?;
        let path = dir.join("settings.json");
        fs::write(
            dir.join("settings.toml"),
            "theme = 'dark'\ncurrent_log_limit = 37\n",
        )?;
        let settings = Settings::load_from(&path)?;
        assert_eq!(settings.theme, ThemeMode::Dark);
        assert_eq!(settings.current_log_limit, 37);
        assert_eq!(settings.logging, LogSettings::default());
        assert!(path.exists());
        fs::write(dir.join("settings.toml"), "theme = 'light'")?;
        assert_eq!(Settings::load_from(&path)?, settings);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn json_validation_rejects_typos_and_invalid_policy() {
        for json in [
            r#"{"theme":"invalid"}"#,
            r#"{"current_log_limit":0}"#,
            r#"{"logging":{"retention_days":0}}"#,
            r#"{"logging":{"check_interval_hours":0}}"#,
            r#"{"logging":{"max_file_mib":4294967295}}"#,
            r#"{"loging":{}}"#,
            "{",
        ] {
            assert!(Settings::from_json(json).is_err(), "{json}");
        }
        assert_eq!(Settings::from_json("{}").unwrap(), Settings::default());
    }
    #[test]
    fn preferences_and_policy_survive_reloading() -> Result<()> {
        let dir = directory("roundtrip");
        let path = dir.join("settings.json");
        assert_eq!(Settings::load_from(&path)?, Settings::default());
        let settings = Settings {
            theme: ThemeMode::Light,
            current_log_limit: 97,
            logging: LogSettings {
                max_file_mib: 3,
                retention_days: 2,
                check_interval_hours: 12,
            },
        };
        settings.save_to(&path)?;
        assert_eq!(Settings::load_from(&path)?, settings);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
