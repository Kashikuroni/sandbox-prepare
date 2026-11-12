use crate::core::LogSettings;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MaintenanceReport {
    pub checked_at: u64,
    pub deleted_files: usize,
    pub freed_bytes: u64,
    pub errors: Vec<String>,
}

/// Owned by the log writer thread. File changes and writes are always serialized.
pub struct LogFiles {
    directory: Option<PathBuf>,
    session: String,
    part: u64,
    file: Option<File>,
    active: Option<PathBuf>,
    bytes: u64,
    policy: LogSettings,
    pub last_report: MaintenanceReport,
}
impl LogFiles {
    pub fn new(directory: Option<PathBuf>, session: String, policy: LogSettings) -> Self {
        let last_report = directory
            .as_ref()
            .and_then(|dir| std::fs::read(dir.join("maintenance.json")).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            directory,
            session,
            policy,
            last_report,
            part: 0,
            file: None,
            active: None,
            bytes: 0,
        }
    }
    pub fn configure(&mut self, policy: LogSettings) {
        self.policy = policy;
    }
    pub fn due(&self) -> bool {
        timestamp().saturating_sub(self.last_report.checked_at) >= self.policy.interval_seconds()
    }
    pub fn wait_time(&self) -> Duration {
        Duration::from_secs(
            self.policy
                .interval_seconds()
                .saturating_sub(timestamp().saturating_sub(self.last_report.checked_at))
                .clamp(1, 60),
        )
    }
    pub fn write(&mut self, line: &[u8]) -> Result<()> {
        if self.file.is_none() || self.bytes >= self.policy.max_bytes() {
            let directory = self
                .directory
                .as_ref()
                .context("Application log directory is unavailable")?;
            std::fs::create_dir_all(directory)?;
            let path = directory.join(format!("{}-{:06}.log", self.session, self.part));
            self.part += 1;
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            self.file = Some(options.open(&path)?);
            self.active = Some(path);
            self.bytes = 0;
        }
        if let Err(error) = self.file.as_mut().unwrap().write_all(line) {
            self.file = None;
            return Err(error.into());
        }
        self.bytes += line.len() as u64;
        Ok(())
    }
    pub fn flush(&self) -> Result<()> {
        if let Some(file) = &self.file {
            file.sync_data()?;
        }
        Ok(())
    }
    pub fn maintain(&mut self, clear: bool) -> MaintenanceReport {
        if clear {
            // Close before removal, including on platforms that cannot unlink open files.
            self.file = None;
            self.active = None;
            self.bytes = 0;
        }
        let mut report = MaintenanceReport {
            checked_at: timestamp(),
            ..Default::default()
        };
        if let Err(error) = self.scan(clear, &mut report) {
            report
                .errors
                .push(format!("Could not inspect log directory: {error:#}"));
        }
        self.last_report = report;
        if let Err(error) = self.save_report() {
            self.last_report
                .errors
                .push(format!("Could not save check result: {error:#}"));
        }
        self.last_report.clone()
    }
    fn scan(&self, clear: bool, report: &mut MaintenanceReport) -> Result<()> {
        let directory = self
            .directory
            .as_ref()
            .context("Application log directory is unavailable")?;
        if !directory.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(directory)? {
            let result = (|| -> Result<()> {
                let entry = entry?;
                let path = entry.path();
                // Never follow symlinks or remove unrelated files from this directory.
                if !entry.file_type()?.is_file()
                    || !owned_log(&path)
                    || self.active.as_ref() == Some(&path)
                {
                    return Ok(());
                }
                let metadata = entry.metadata()?;
                let expired = SystemTime::now()
                    .duration_since(metadata.modified()?)
                    .is_ok_and(|age| age > self.policy.retention());
                if clear || expired {
                    std::fs::remove_file(&path).with_context(|| {
                        format!("Could not delete {}", entry.file_name().to_string_lossy())
                    })?;
                    report.deleted_files += 1;
                    report.freed_bytes += metadata.len();
                }
                Ok(())
            })();
            if let Err(error) = result {
                report.errors.push(format!("{error:#}"));
            }
        }
        Ok(())
    }
    fn save_report(&self) -> Result<()> {
        let directory = self
            .directory
            .as_ref()
            .context("Application log directory is unavailable")?;
        std::fs::create_dir_all(directory)?;
        let temp = directory.join("maintenance.json.tmp");
        std::fs::write(&temp, serde_json::to_vec(&self.last_report)?)?;
        std::fs::rename(temp, directory.join("maintenance.json"))?;
        Ok(())
    }
}
fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn owned_log(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "log")
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let parts: Vec<_> = name.split('-').collect();
                parts.len() == 3
                    && parts
                        .iter()
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sandbox-files-{label}-{}-{timestamp}",
            std::process::id(),
            timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    fn age(path: &Path, days: u64) -> Result<()> {
        File::options().write(true).open(path)?.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(days * 86400)),
        )?;
        Ok(())
    }
    #[test]
    fn policy_changes_apply_to_deadline_size_and_retention() -> Result<()> {
        let dir = dir("policy");
        let mut writer = LogFiles::new(
            Some(dir.clone()),
            "100-12".into(),
            LogSettings {
                max_file_mib: 2,
                retention_days: 30,
                check_interval_hours: 48,
            },
        );
        writer.last_report.checked_at = timestamp() - 25 * 3600;
        assert!(!writer.due());
        writer.write(&vec![b'x'; 1024 * 1024 + 9])?;
        let previous = writer.active.clone().unwrap();
        writer.configure(LogSettings {
            max_file_mib: 1,
            ..Default::default()
        });
        assert!(writer.due());
        writer.write(b"next\n")?;
        assert_ne!(writer.active.as_ref(), Some(&previous));
        age(&previous, 8)?;
        let report = writer.maintain(false);
        assert!(report.errors.is_empty());
        assert_eq!(report.deleted_files, 1);
        assert!(!previous.exists());
        drop(writer);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn retention_preserves_active_recent_foreign_and_future_files() -> Result<()> {
        let dir = dir("retention");
        let mut writer = LogFiles::new(Some(dir.clone()), "100-10".into(), LogSettings::default());
        writer.write(b"active\n")?;
        let active = writer.active.clone().unwrap();
        age(&active, 9)?;
        let old = dir.join("99-10-000000.log");
        std::fs::write(&old, b"old")?;
        age(&old, 8)?;
        let recent = dir.join("99-10-000001.log");
        std::fs::write(&recent, b"recent")?;
        let future = dir.join("99-10-000002.log");
        std::fs::write(&future, b"future")?;
        File::options().write(true).open(&future)?.set_times(
            std::fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(86400)),
        )?;
        let foreign = dir.join("notes.log");
        std::fs::write(&foreign, b"keep")?;
        age(&foreign, 100)?;
        let report = writer.maintain(false);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.deleted_files, 1);
        assert_eq!(report.freed_bytes, 3);
        assert!(!old.exists());
        for path in [&active, &recent, &future, &foreign] {
            assert!(path.exists());
        }
        assert!(!writer.due());
        drop(writer);
        let restored = LogFiles::new(Some(dir.clone()), "101-10".into(), LogSettings::default());
        assert!(!restored.due());
        drop(restored);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn rotation_keeps_whole_records_and_clear_starts_new_file() -> Result<()> {
        let dir = dir("rotate");
        let mut writer = LogFiles::new(
            Some(dir.clone()),
            "100-11".into(),
            LogSettings {
                max_file_mib: 1,
                ..Default::default()
            },
        );
        let record = vec![b'x'; 1024 * 1024 + 9];
        writer.write(&record)?;
        writer.write(b"next\n")?;
        assert_eq!(std::fs::read(dir.join("100-11-000000.log"))?, record);
        assert_eq!(std::fs::read(dir.join("100-11-000001.log"))?, b"next\n");
        let report = writer.maintain(true);
        assert!(report.errors.is_empty());
        assert_eq!(report.deleted_files, 2);
        writer.write(b"after clear\n")?;
        assert_eq!(
            std::fs::read(dir.join("100-11-000002.log"))?,
            b"after clear\n"
        );
        drop(writer);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
