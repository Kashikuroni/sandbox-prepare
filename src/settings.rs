use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::theme::ThemeMode;

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    pub current_log_limit: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::default(),
            current_log_limit: 2000,
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

    fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let settings: Self = toml::from_str(&contents).context("Invalid settings file")?;
                if settings.current_log_limit == 0 {
                    return Err(anyhow!("Log limit must be greater than zero"));
                }
                Ok(settings)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("Could not read settings"),
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self)?;
        let parent = path.parent().context("Settings directory is missing")?;
        fs::create_dir_all(parent).context("Could not create settings directory")?;
        // Write beside the destination, then replace it atomically.
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, contents).context("Could not write settings")?;
        fs::rename(&temporary, path).context("Could not replace settings file")
    }
}

fn settings_path() -> Result<PathBuf> {
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
    Ok(config_dir.join("sandbox-prepare/settings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_theme_defaults_to_system_and_invalid_theme_is_reported() {
        assert_eq!(
            toml::from_str::<Settings>("").unwrap().theme,
            ThemeMode::System
        );
        assert!(toml::from_str::<Settings>("theme = 'invalid'").is_err());
        assert!(toml::from_str::<Settings>("theme = [").is_err());
    }

    #[test]
    fn log_limit_defaults_and_survives_restart() -> Result<()> {
        assert_eq!(
            toml::from_str::<Settings>("theme = 'dark'")?.current_log_limit,
            2000
        );
        let directory =
            env::temp_dir().join(format!("sandbox-log-settings-{}", std::process::id()));
        let path = directory.join("settings.toml");
        Settings {
            current_log_limit: 37,
            ..Default::default()
        }
        .save_to(&path)?;
        assert_eq!(Settings::load_from(&path)?.current_log_limit, 37);
        fs::write(&path, "current_log_limit = 0")?;
        assert!(Settings::load_from(&path).is_err());
        fs::remove_dir_all(directory)?;
        Ok(())
    }
    #[test]
    fn preferences_survive_reloading_and_replacing_the_file() -> Result<()> {
        let directory =
            env::temp_dir().join(format!("sandbox-prepare-settings-{}", std::process::id()));
        let path = directory.join("settings.toml");
        assert_eq!(Settings::load_from(&path)?.theme, ThemeMode::System);
        for theme in ThemeMode::ALL {
            Settings {
                theme,
                ..Default::default()
            }
            .save_to(&path)?;
            assert_eq!(Settings::load_from(&path)?.theme, theme);
            assert_eq!(Settings::load_from(&path)?.current_log_limit, 2000);
        }
        fs::remove_dir_all(directory)?;
        Ok(())
    }
}
