//! Container/helper output, deliberately separate from the application's debug log.
use crate::{log_files::LogFiles, settings::LogSettings};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};
#[derive(Clone, Serialize, Deserialize)]
pub struct Line {
    pub seq: u64,
    pub time: String,
    pub process: String,
    pub text: String,
}
pub struct ProcessLog {
    lines: Mutex<VecDeque<Line>>,
    files: Mutex<LogFiles>,
    pub revision: AtomicU64,
    sequence: AtomicU64,
    limit: AtomicUsize,
    secrets: Vec<String>,
}
impl ProcessLog {
    pub fn new(directory: PathBuf, secrets: Vec<String>) -> Self {
        let session = format!(
            "{}-{}",
            chrono::Utc::now().timestamp_millis(),
            std::process::id()
        );
        Self {
            lines: Mutex::new(VecDeque::new()),
            files: Mutex::new(LogFiles::new(
                Some(directory),
                session,
                LogSettings::default(),
            )),
            revision: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            limit: AtomicUsize::new(2000),
            secrets,
        }
    }
    pub fn configure(&self, limit: usize, policy: LogSettings) {
        self.limit.store(limit.max(1), Ordering::Release);
        let mut lines = self.lines.lock().unwrap();
        while lines.len() > limit.max(1) {
            lines.pop_front();
        }
        self.files.lock().unwrap().configure(policy);
        self.revision.fetch_add(1, Ordering::Release);
    }
    pub fn append(&self, process: &str, mut text: String) {
        for secret in &self.secrets {
            text = text.replace(secret, "[redacted]");
        }
        let mut lines = self.lines.lock().unwrap();
        let line = Line {
            seq: self.sequence.fetch_add(1, Ordering::Relaxed),
            time: chrono::Local::now().format("%d.%m.%Y %H:%M:%S").to_string(),
            process: process.into(),
            text,
        };
        let mut bytes = serde_json::to_vec(&line).unwrap_or_default();
        bytes.push(b'\n');
        lines.push_back(line);
        while lines.len() > self.limit.load(Ordering::Acquire) {
            lines.pop_front();
        }
        drop(lines);
        let mut files = self.files.lock().unwrap();
        if files.due() {
            files.maintain(false);
        }
        if let Err(error) = files.write(&bytes) {
            tracing::error!(target:"app::storage",%error,"Could not write process logs");
        }
        self.revision.fetch_add(1, Ordering::Release);
    }
    pub fn snapshot(&self) -> Vec<Line> {
        self.lines.lock().unwrap().iter().cloned().collect()
    }
    pub fn flush(&self) {
        let _ = self.files.lock().unwrap().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn container_secrets_are_redacted_before_memory_and_file() -> anyhow::Result<()> {
        let root =
            std::env::temp_dir().join(format!("sandbox-process-log-{}", crate::docker::nonce()?));
        let secret = "private-test-password";
        let logs = ProcessLog::new(root.clone(), vec![secret.into()]);
        logs.append("mysql", format!("first line\nsecond line {secret}"));
        logs.flush();
        assert_eq!(
            logs.snapshot()[0].text,
            "first line\nsecond line [redacted]"
        );
        let file = std::fs::read_dir(&root)?
            .find_map(|e| {
                let path = e.ok()?.path();
                (path.extension()?.to_str() == Some("log")).then_some(path)
            })
            .unwrap();
        assert!(!std::fs::read_to_string(file)?.contains(secret));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
