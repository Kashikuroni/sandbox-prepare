use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub session_id: String,
    pub seq: u64,
    pub timestamp: String,
    pub level: String,
    pub category: String,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}
impl Entry {
    pub fn json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}
struct State {
    entries: VecDeque<Arc<Entry>>,
    limit: usize,
    seq: u64,
    evicted: u64,
    secrets: Vec<String>,
}
enum WriteEvent {
    Entry(Arc<Entry>),
    Flush(mpsc::Sender<()>),
    Shutdown,
    Configure(crate::core::LogSettings),
    Maintain {
        clear: bool,
        done: mpsc::Sender<crate::log_files::MaintenanceReport>,
    },
}
pub struct LogStore {
    state: Mutex<State>,
    revision: AtomicU64,
    session_id: String,
    directory: Option<PathBuf>,
    sender: mpsc::SyncSender<WriteEvent>,
    disk_error: Arc<Mutex<Option<String>>>,
    history_revision: Arc<AtomicU64>,
    maintenance: Arc<Mutex<crate::log_files::MaintenanceReport>>,
    writer: Option<thread::JoinHandle<()>>,
}
static STORE: OnceLock<Arc<LogStore>> = OnceLock::new();
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
fn now() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}
impl LogStore {
    #[cfg(test)]
    pub fn new(directory: Option<PathBuf>, limit: usize) -> Arc<Self> {
        Self::with_policy(directory, limit, crate::core::LogSettings::default())
    }
    pub fn with_policy(
        directory: Option<PathBuf>,
        limit: usize,
        policy: crate::core::LogSettings,
    ) -> Arc<Self> {
        let session_id = format!("{}-{}", now().as_nanos(), std::process::id());
        let disk_error = Arc::new(Mutex::new(None));
        let (sender, receiver) = mpsc::sync_channel::<WriteEvent>(1024);
        let error = disk_error.clone();
        let dir = directory.clone();
        let session = session_id.clone();
        let history_revision = Arc::new(AtomicU64::new(0));
        let history_changed = history_revision.clone();
        let maintenance = Arc::new(Mutex::new(crate::log_files::MaintenanceReport::default()));
        let status = maintenance.clone();
        let writer = thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || {
                let mut files = crate::log_files::LogFiles::new(dir, session, policy);
                *lock(&status) = files.last_report.clone();
                loop {
                    if files.due() {
                        let report = files.maintain(false);
                        if report.deleted_files > 0 {
                            history_changed.fetch_add(1, Ordering::Relaxed);
                        }
                        *lock(&status) = report;
                    }
                    let event = match receiver.recv_timeout(files.wait_time()) {
                        Ok(event) => event,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match event {
                        WriteEvent::Shutdown => {
                            let _ = files.flush();
                            break;
                        }
                        WriteEvent::Entry(entry) => {
                            let result = (|| -> Result<()> {
                                let mut line = serde_json::to_vec(&*entry)?;
                                line.push(b'\n');
                                files.write(&line)
                            })();
                            if let Err(failure) = result {
                                *lock(&error) = Some(format!("Log file write failed: {failure}"));
                            }
                        }
                        WriteEvent::Configure(policy) => files.configure(policy),
                        WriteEvent::Maintain { clear, done } => {
                            let report = files.maintain(clear);
                            if clear || report.deleted_files > 0 {
                                history_changed.fetch_add(1, Ordering::Relaxed);
                            }
                            *lock(&status) = report.clone();
                            let _ = done.send(report);
                        }
                        WriteEvent::Flush(done) => {
                            if let Err(failure) = files.flush() {
                                *lock(&error) = Some(format!("Log file sync failed: {failure}"));
                            }
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("Could not start log writer");
        Arc::new(Self {
            state: Mutex::new(State {
                entries: VecDeque::new(),
                limit: limit.max(1),
                seq: 0,
                evicted: 0,
                secrets: vec![],
            }),
            revision: AtomicU64::new(0),
            session_id,
            directory,
            sender,
            disk_error,
            history_revision,
            maintenance,
            writer: Some(writer),
        })
    }
    pub fn configure(&self, policy: crate::core::LogSettings) -> Result<()> {
        self.sender
            .send(WriteEvent::Configure(policy))
            .context("Log writer stopped")
    }
    pub fn history_revision(&self) -> u64 {
        self.history_revision.load(Ordering::Relaxed)
    }
    pub fn maintenance_status(&self) -> crate::log_files::MaintenanceReport {
        lock(&self.maintenance).clone()
    }
    pub fn maintain(&self, clear: bool) -> Result<crate::log_files::MaintenanceReport> {
        // Holding the sequencing lock also prevents new entries racing a full reset.
        let mut state = clear.then(|| lock(&self.state));
        let (done, wait) = mpsc::channel();
        self.sender
            .send(WriteEvent::Maintain { clear, done })
            .context("Log writer stopped")?;
        let report = wait.recv().context("Log writer stopped")?;
        if let Some(state) = &mut state {
            state.entries.clear();
            state.evicted = 0;
            self.revision.fetch_add(1, Ordering::Relaxed);
            *lock(&self.disk_error) = None;
        }
        Ok(report)
    }
    pub fn snapshot(&self) -> Vec<Arc<Entry>> {
        lock(&self.state).entries.iter().cloned().collect()
    }
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
    pub fn evicted(&self) -> u64 {
        lock(&self.state).evicted
    }
    pub fn directory(&self) -> Option<PathBuf> {
        self.directory.clone()
    }
    pub fn disk_error(&self) -> Option<String> {
        lock(&self.disk_error).clone()
    }
    pub fn set_limit(&self, limit: usize) {
        let mut state = lock(&self.state);
        state.limit = limit.max(1);
        while state.entries.len() > state.limit {
            state.entries.pop_front();
            state.evicted += 1;
        }
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
    pub fn register_secret(&self, secret: &str) {
        if !secret.is_empty() {
            let mut state = lock(&self.state);
            if !state.secrets.iter().any(|s| s == secret) {
                state.secrets.push(secret.into());
                state.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
            }
        }
    }
    fn append(
        &self,
        level: &str,
        category: &str,
        mut message: String,
        mut fields: BTreeMap<String, String>,
    ) {
        let mut state = lock(&self.state);
        for secret in &state.secrets {
            message = message.replace(secret, "[redacted]");
            for value in fields.values_mut() {
                *value = value.replace(secret, "[redacted]");
            }
        }
        state.seq += 1;
        let entry = Arc::new(Entry {
            session_id: self.session_id.clone(),
            seq: state.seq,
            timestamp: aws_sdk_s3::primitives::DateTime::from_millis(now().as_millis() as i64)
                .to_string(),
            level: level.into(),
            category: category.into(),
            message,
            fields,
        });
        state.entries.push_back(entry.clone());
        while state.entries.len() > state.limit {
            state.entries.pop_front();
            state.evicted += 1;
        }
        // Sending under the sequencing lock preserves identical ordering in both sources.
        if self.sender.send(WriteEvent::Entry(entry)).is_err() {
            *lock(&self.disk_error) = Some("Log writer stopped".into());
        }
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
    pub fn flush(&self) -> Result<()> {
        let (done, wait) = mpsc::channel();
        self.sender
            .send(WriteEvent::Flush(done))
            .context("Log writer stopped")?;
        wait.recv().context("Log writer stopped")?;
        if let Some(error) = self.disk_error() {
            anyhow::bail!(error);
        }
        Ok(())
    }
}
impl Drop for LogStore {
    fn drop(&mut self) {
        let _ = self.sender.send(WriteEvent::Shutdown);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}
pub fn store() -> Arc<LogStore> {
    STORE
        .get()
        .expect("Logging must be initialized first")
        .clone()
}
pub fn register_credentials(access: &str, secret: &str) {
    if let Some(store) = STORE.get() {
        store.register_secret(access);
        store.register_secret(secret);
    }
}
fn sensitive(name: &str) -> bool {
    let name = name.to_lowercase();
    [
        "secret",
        "password",
        "access_key",
        "token",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|part| name.contains(part))
}
#[derive(Default)]
struct Fields {
    message: String,
    fields: BTreeMap<String, String>,
}
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let value = if sensitive(field.name()) {
            "[redacted]".into()
        } else {
            format!("{value:?}")
        };
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name().into(), value);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.into();
        } else {
            self.fields.insert(
                field.name().into(),
                if sensitive(field.name()) {
                    "[redacted]".into()
                } else {
                    value.into()
                },
            );
        }
    }
}
struct AppLayer(Arc<LogStore>);
impl<S: Subscriber> Layer<S> for AppLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with("app::") {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.append(
            meta.level().as_str(),
            meta.target().trim_start_matches("app::"),
            fields.message,
            fields.fields,
        );
    }
}
pub fn init(directory: Option<PathBuf>, limit: usize, policy: crate::core::LogSettings) {
    let logger = LogStore::with_policy(directory, limit, policy);
    let _ = STORE.set(logger.clone());
    tracing_subscriber::registry().with(AppLayer(logger)).init();
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(target:"app::panic","{info}");
        let _ = store().flush();
        eprintln!("Application panicked; see application logs");
    }));
    tracing::info!(target:"app::startup",version=env!("CARGO_PKG_VERSION"),"Application started");
}

#[derive(Clone, Default)]
pub struct Filter {
    pub category: Option<String>,
    pub level: Option<String>,
}
impl Filter {
    pub fn matches(&self, entry: &Entry) -> bool {
        self.category.as_ref().is_none_or(|v| *v == entry.category)
            && self.level.as_ref().is_none_or(|v| *v == entry.level)
    }
}
#[derive(Clone, Debug)]
pub struct Cursor {
    pub file: PathBuf,
    pub offset: u64,
}
pub struct Page {
    pub entries: Vec<Arc<Entry>>,
    pub next: Option<Cursor>,
    pub malformed: usize,
}
pub fn history_page(
    directory: &Path,
    cursor: Option<Cursor>,
    filter: &Filter,
    page_size: usize,
) -> Result<Page> {
    if !directory.exists() {
        return Ok(Page {
            entries: vec![],
            next: None,
            malformed: 0,
        });
    }
    let mut files = std::fs::read_dir(directory)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.retain(|p| p.extension().is_some_and(|extension| extension == "log"));
    files.sort();
    let start = cursor
        .as_ref()
        .map(|c| {
            files
                .iter()
                .position(|p| *p == c.file)
                .context("History file no longer exists")
        })
        .transpose()?
        .unwrap_or(0);
    let mut page = Page {
        entries: vec![],
        next: None,
        malformed: 0,
    };
    for (index, path) in files.iter().enumerate().skip(start) {
        let mut reader = BufReader::new(File::open(path)?);
        if index == start {
            reader.seek(SeekFrom::Start(cursor.as_ref().map_or(0, |c| c.offset)))?;
        }
        loop {
            let offset = reader.stream_position()?;
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            // A writer may currently be appending the final line. Retry it on Refresh.
            if !line.ends_with('\n') {
                break;
            }
            if page.entries.len() >= page_size.max(1) {
                page.next = Some(Cursor {
                    file: path.clone(),
                    offset,
                });
                return Ok(page);
            }
            match serde_json::from_str::<Entry>(&line) {
                Ok(entry) if filter.matches(&entry) => page.entries.push(Arc::new(entry)),
                Ok(_) => {}
                Err(_) => page.malformed += 1,
            }
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::OpenOptions, io::Write};

    #[test]
    fn clearing_resets_current_and_history_without_reusing_sequence_numbers() -> Result<()> {
        let directory = directory("clear");
        let store = LogStore::new(Some(directory.clone()), 2);
        for _ in 0..3 {
            store.append("INFO", "ui", "before".into(), BTreeMap::new());
        }
        let generation = store.history_revision();
        let report = store.maintain(true)?;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.deleted_files, 1);
        assert!(store.snapshot().is_empty());
        assert_eq!(store.evicted(), 0);
        assert!(store.history_revision() > generation);
        assert!(
            history_page(&directory, None, &Filter::default(), 10)?
                .entries
                .is_empty()
        );
        store.append("INFO", "ui", "after".into(), BTreeMap::new());
        store.flush()?;
        let page = history_page(&directory, None, &Filter::default(), 10)?;
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].message, "after");
        assert_eq!(page.entries[0].seq, 4);
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
    fn directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sandbox-logs-{label}-{}-{}",
            std::process::id(),
            now().as_nanos()
        ))
    }
    #[test]
    fn tracing_fields_are_structured_and_sensitive_fields_are_masked() -> Result<()> {
        let directory = directory("fields");
        let logger = LogStore::new(Some(directory.clone()), 10);
        let subscriber = tracing_subscriber::registry().with(AppLayer(logger.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target:"app::storage",access_key="not-registered",bucket="ms-bucket",count=3,"Saved");
        });
        logger.flush()?;
        let page = history_page(&directory, None, &Filter::default(), 10)?;
        assert_eq!(page.entries[0].fields["access_key"], "[redacted]");
        assert_eq!(page.entries[0].fields["bucket"], "ms-bucket");
        drop(logger);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
    #[test]
    fn concurrent_events_keep_the_same_order_in_memory_and_file() -> Result<()> {
        let directory = directory("threads");
        let logger = LogStore::new(Some(directory.clone()), 100);
        let threads: Vec<_> = (0..4)
            .map(|worker| {
                let logger = logger.clone();
                std::thread::spawn(move || {
                    for index in 0..20 {
                        logger.append("DEBUG", "ui", format!("{worker}/{index}"), BTreeMap::new());
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        logger.flush()?;
        let memory = logger.snapshot();
        let history = history_page(&directory, None, &Filter::default(), 100)?;
        assert_eq!(memory.len(), 80);
        for (index, (a, b)) in memory.iter().zip(&history.entries).enumerate() {
            assert_eq!(a.seq, (index + 1) as u64);
            assert_eq!(a.seq, b.seq);
            assert_eq!(a.message, b.message);
        }
        let next_session = LogStore::new(Some(directory.clone()), 100);
        assert!(next_session.snapshot().is_empty());
        drop(next_session);
        drop(logger);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
    #[test]
    fn current_is_bounded_history_is_full_and_messages_are_not_truncated() -> Result<()> {
        let directory = directory("retention");
        let store = LogStore::new(Some(directory.clone()), 2);
        let full = "long message\n".repeat(10000);
        for index in 0..5 {
            store.append(
                "INFO",
                "app",
                if index == 4 {
                    full.clone()
                } else {
                    index.to_string()
                },
                BTreeMap::new(),
            );
        }
        assert_eq!(store.snapshot().len(), 2);
        assert_eq!(store.evicted(), 3);
        assert_eq!(store.snapshot()[1].message, full);
        store.flush()?;
        let page = history_page(&directory, None, &Filter::default(), 3)?;
        assert_eq!(page.entries.len(), 3);
        let page = history_page(&directory, page.next, &Filter::default(), 3)?;
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[1].message, full);
        store.set_limit(1);
        assert_eq!(store.snapshot()[0].seq, 5);
        store.set_limit(10);
        assert_eq!(store.snapshot().len(), 1);
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
    #[test]
    fn secrets_are_redacted_before_both_destinations() -> Result<()> {
        let directory = directory("secrets");
        let store = LogStore::new(Some(directory.clone()), 10);
        store.register_secret("test-secret");
        store.append(
            "ERROR",
            "s3",
            "failure test-secret".into(),
            BTreeMap::from([("reason".into(), "test-secret".into())]),
        );
        store.flush()?;
        let page = history_page(&directory, None, &Filter::default(), 10)?;
        assert_eq!(page.entries[0].message, "failure [redacted]");
        assert_eq!(page.entries[0].fields["reason"], "[redacted]");
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
    #[test]
    fn history_filters_skips_bad_lines_and_preserves_unfinished_line() -> Result<()> {
        let directory = directory("parse");
        let store = LogStore::new(Some(directory.clone()), 10);
        store.append("INFO", "app", "ready".into(), BTreeMap::new());
        store.append("ERROR", "s3", "failed".into(), BTreeMap::new());
        store.flush()?;
        let path = std::fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "log"))
            .unwrap();
        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(b"broken\n{\"unfinished\"")?;
        let page = history_page(
            &directory,
            None,
            &Filter {
                category: Some("s3".into()),
                level: None,
            },
            10,
        )?;
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.malformed, 1);
        drop(file);
        drop(store);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
