use crate::theme::ThemePalette as _;
use crate::{
    backup_cache::{self, Cache, Progress, SOURCES, Snapshot},
    backups::{Backup, DownloadError, StorageConfig, format_size},
    icons::Icon,
    theme::{Palette, ThemeMode},
};
use gpui::{Context, Div, Render, Window, div, prelude::*, px, rgb};
use sandbox_backups::service::Service;
use std::sync::{Arc, atomic::Ordering};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

enum Transfer {
    Idle,
    Running {
        bytes: u64,
        speed: u64,
        started: Instant,
    },
    Verifying {
        bytes: u64,
        total: u64,
    },
    WaitingForVerification,
    VerificationFailed,
    Verified,
}
struct Row {
    expanded: bool,
    loading: bool,
    remote: Option<Backup>,
    error: Option<String>,
    transfer: Transfer,
}
enum Event {
    Local(Result<Snapshot, String>),
    Listed(usize, Result<Option<Backup>, String>),
    Progress(Progress),
    Finished(Result<Snapshot, DownloadError>),
    IntegrityFinished(Result<Snapshot, DownloadError>),
}
pub struct BackupsView {
    config: StorageConfig,
    service: Arc<Service>,
    revision: u64,
    rows: [Row; 2],
    events: mpsc::Sender<Event>,
    theme: ThemeMode,
    cache: Option<Cache>,
    local: Snapshot,
    local_loading: bool,
    local_error: Option<String>,
    cancel: Option<backup_cache::UpdateControl>,
    integrity_check: bool,
    cancelling: bool,
    publishing: bool,
    message: Option<String>,
}
impl BackupsView {
    pub fn new(service: Arc<Service>, theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        let (events, receiver) = mpsc::channel();
        cx.spawn(async move |view, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(100)).await;
                let pending: Vec<_> = receiver.try_iter().collect();
                if view
                    .update(cx, |view, cx| {
                        let revision = view.service.revision.load(Ordering::Acquire);
                        if revision != view.revision && !view.busy() {
                            view.revision = revision;
                            view.config = view.service.project().storage;
                            view.check(cx);
                        }
                        if pending.is_empty() {
                            return;
                        }
                        for event in pending {
                            view.handle(event, cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let cache = service.cache.clone();
        let local_error = None;
        let mut view = Self {
            config: service.project().storage,
            revision: service.revision.load(Ordering::Acquire),
            service,
            theme,
            events,
            rows: std::array::from_fn(|_| Row {
                expanded: false,
                loading: false,
                remote: None,
                error: None,
                transfer: Transfer::Idle,
            }),
            cache: Some(cache),
            local: Snapshot::default(),
            local_loading: false,
            local_error,
            cancel: None,
            integrity_check: false,
            cancelling: false,
            publishing: false,
            message: None,
        };
        view.check(cx);
        view
    }
    fn busy(&self) -> bool {
        self.cancel.is_some() || self.local_loading || self.rows.iter().any(|row| row.loading)
    }
    pub fn service(&self) -> Arc<Service> {
        self.service.clone()
    }
    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }
    fn remote(&self) -> Option<[Backup; 2]> {
        Some([self.rows[0].remote.clone()?, self.rows[1].remote.clone()?])
    }
    fn current(&self) -> bool {
        self.remote()
            .is_some_and(|remote| self.local.current(&self.config, &remote))
    }
    fn actionable(&self) -> bool {
        !self.busy()
            && self.local_error.is_none()
            && self.cache.is_some()
            && self
                .rows
                .iter()
                .all(|row| row.error.is_none() && row.remote.is_some())
            && !self.current()
    }
    fn check_remote(&mut self) {
        for index in 0..2 {
            self.rows[index].loading = true;
            self.rows[index].error = None;
            let events = self.events.clone();
            if let Err(error) = self.service.check_remote(index, move |result| {
                let _ = events.send(Event::Listed(index, result));
            }) {
                self.rows[index].loading = false;
                self.rows[index].error = Some(error.to_string());
            }
        }
    }
    fn check(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.message = None;
        self.local_loading = true;
        let events = self.events.clone();
        if let Err(error) = self.service.check_local(move |result| {
            let _ = events.send(Event::Local(result));
        }) {
            self.local_loading = false;
            self.local_error = Some(error.to_string());
        }
        self.check_remote();
        cx.notify();
    }
    fn download(&mut self, cx: &mut Context<Self>) {
        if !self.actionable() {
            return;
        }
        let Some(remote) = self.remote() else {
            return;
        };
        self.cancelling = false;
        self.publishing = false;
        self.message = None;
        for row in &mut self.rows {
            row.transfer = Transfer::Idle;
        }
        let events = self.events.clone();
        let finish = self.events.clone();
        match self.service.download(
            remote,
            move |event| {
                let _ = events.send(Event::Progress(event));
            },
            move |result| {
                let _ = finish.send(Event::Finished(result));
            },
        ) {
            Ok(control) => self.cancel = Some(control),
            Err(error) => self.message = Some(error.to_string()),
        }
        cx.notify();
    }
    fn verify_integrity(&mut self, cx: &mut Context<Self>) {
        if self.busy() || self.local.pair.is_none() {
            return;
        }
        self.integrity_check = true;
        self.cancelling = false;
        self.publishing = false;
        self.message = None;
        for row in &mut self.rows {
            row.transfer = Transfer::Idle;
        }
        let events = self.events.clone();
        let finish = self.events.clone();
        match self.service.verify(
            move |event| {
                let _ = events.send(Event::Progress(event));
            },
            move |result| {
                let _ = finish.send(Event::IntegrityFinished(result));
            },
        ) {
            Ok(control) => self.cancel = Some(control),
            Err(error) => {
                self.integrity_check = false;
                self.message = Some(error.to_string());
            }
        }
        cx.notify();
    }
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.publishing
            && let Some(cancel) = &self.cancel
        {
            if cancel.cancel() {
                self.cancelling = true;
            } else {
                self.publishing = true;
            }
            cx.notify();
        }
    }
    fn handle(&mut self, event: Event, _: &mut Context<Self>) {
        match event {
            Event::Local(result) => {
                self.local_loading = false;
                match result {
                    Ok(local) => {
                        self.local = local;
                        self.local_error = None;
                    }
                    Err(error) => self.local_error = Some(error),
                }
            }
            Event::Listed(index, result) => {
                let row = &mut self.rows[index];
                row.loading = false;
                match result {
                    Ok(remote) => row.remote = remote,
                    Err(error) => {
                        row.remote = None;
                        row.error = Some(error);
                    }
                }
            }
            Event::Progress(progress) => match progress {
                Progress::Started(index) => {
                    self.rows[index].transfer = Transfer::Running {
                        bytes: 0,
                        speed: 0,
                        started: Instant::now(),
                    };
                    tracing::info!(target:"app::download",source=SOURCES[index].0,"Backup download started");
                }
                Progress::Bytes(index, bytes) => {
                    if let Transfer::Running {
                        bytes: current,
                        speed,
                        started,
                    } = &mut self.rows[index].transfer
                    {
                        *current = bytes;
                        *speed = (bytes as f64 / started.elapsed().as_secs_f64().max(0.1)) as u64;
                    }
                }
                Progress::WaitingForVerification(index) => {
                    self.rows[index].transfer = Transfer::WaitingForVerification;
                }
                Progress::Verifying(index) => {
                    let total = if self.integrity_check {
                        self.local
                            .pair
                            .as_ref()
                            .map_or(0, |pair| pair.records[index].size)
                    } else {
                        self.rows[index]
                            .remote
                            .as_ref()
                            .map_or(0, |remote| remote.size)
                    };
                    self.rows[index].transfer = Transfer::Verifying { bytes: 0, total };
                    tracing::info!(target:"app::download",source=SOURCES[index].0,"Verifying backup archive");
                }
                Progress::VerificationBytes(index, bytes, total) => {
                    self.rows[index].transfer = Transfer::Verifying { bytes, total };
                }
                Progress::VerificationFailed(index) => {
                    self.rows[index].transfer = Transfer::VerificationFailed
                }
                Progress::Verified(index) => self.rows[index].transfer = Transfer::Verified,
                Progress::Publishing => self.publishing = true,
                Progress::WaitingForReaders => {
                    self.message =
                        Some("Waiting for restores to release the current backups…".into())
                }
            },
            Event::IntegrityFinished(result) => {
                self.cancel = None;
                self.integrity_check = false;
                self.cancelling = false;
                self.publishing = false;
                for row in &mut self.rows {
                    row.transfer = Transfer::Idle;
                }
                match result {
                    Ok(local) => {
                        let valid = !local.missing();
                        self.local = local;
                        self.local_error = None;
                        self.message = Some(
                            if valid {
                                "Both local backups passed integrity verification."
                            } else {
                                "Integrity verification failed. Download the backups again."
                            }
                            .into(),
                        );
                        if valid {
                            tracing::info!(target:"app::download", "Local backup integrity verified");
                        } else {
                            tracing::error!(target:"app::download", "Local backup integrity verification failed");
                        }
                    }
                    Err(DownloadError::Cancelled) => {
                        self.message =
                            Some("Verification cancelled. Local files are unchanged.".into());
                        tracing::info!(target:"app::download", "Local verification cancelled");
                    }
                    Err(DownloadError::Failed(error)) => {
                        self.local_error = Some(format!("Could not verify local backups: {error}"));
                        tracing::error!(target:"app::download", %error, "Could not verify local backups");
                    }
                }
            }
            Event::Finished(result) => {
                self.cancel = None;
                self.cancelling = false;
                self.publishing = false;
                for row in &mut self.rows {
                    row.transfer = Transfer::Idle;
                }
                match result {
                    Ok(local) => {
                        self.local = local;
                        self.message = Some("Both backups downloaded and verified.".into());
                        tracing::info!(target:"app::download","Verified backup pair activated");
                        self.check_remote();
                    }
                    Err(DownloadError::Cancelled) => {
                        self.message =
                            Some("Download cancelled. Current local backups are unchanged.".into());
                        tracing::info!(target:"app::download","Backup update cancelled");
                    }
                    Err(DownloadError::Failed(error)) => {
                        tracing::error!(target:"app::download",%error,"Backup update failed; previous pair retained");
                        self.message = Some(format!(
                            "Update failed: {error}. Current local backups are unchanged."
                        ));
                    }
                }
            }
        }
    }
    fn row_status(&self, index: usize) -> &str {
        let row = &self.rows[index];
        match row.transfer {
            Transfer::Running { .. } => return "Downloading…",
            Transfer::WaitingForVerification => return "Downloaded · waiting for verification",
            Transfer::Verifying { .. } => return "Verifying archive…",
            Transfer::VerificationFailed => return "Verification failed",
            Transfer::Verified => {
                return if self.integrity_check {
                    "Verified"
                } else {
                    "Verified · waiting for pair"
                };
            }
            Transfer::Idle => {}
        }
        if self.local_loading {
            "Checking local copy…"
        } else if row.loading {
            "Checking S3…"
        } else if row.error.is_some() {
            "S3 unavailable · freshness unknown"
        } else if self.local_error.is_some() {
            "Local check failed"
        } else if self.local.pair.is_some() && !self.local.valid[index] {
            "Local file changed · verify or download"
        } else if !self.local.valid[index] {
            "Not downloaded"
        } else if row.remote.is_none() {
            "No backup in S3"
        } else if self.local.pair.as_ref().is_some_and(|pair| {
            pair.origin == backup_cache::Origin::new(&self.config)
                && row
                    .remote
                    .as_ref()
                    .is_some_and(|remote| pair.records[index].matches(remote))
        }) {
            "Up to date"
        } else {
            "Update available"
        }
    }

    fn row(&self, index: usize, palette: Palette, cx: &Context<Self>) -> Div {
        let row = &self.rows[index];
        let local = self.local.pair.as_ref().map(|pair| &pair.records[index]);
        let local_summary = if self.local_loading {
            "Checking…".into()
        } else if self.local_error.is_some() {
            "Unavailable".into()
        } else if let Some(record) = local.filter(|_| self.local.valid[index]) {
            compact_metadata(&record.modified, record.size)
        } else if local.is_some() {
            "Invalid".into()
        } else {
            "Missing".into()
        };
        let remote_summary = if row.loading {
            "Checking…".into()
        } else if row.error.is_some() {
            "Unavailable".into()
        } else if let Some(remote) = &row.remote {
            compact_metadata(&remote.modified.to_string(), remote.size)
        } else {
            "Missing".into()
        };
        let status_error = matches!(row.transfer, Transfer::VerificationFailed)
            || row.error.is_some()
            || self.local_error.is_some()
            || (!self.local_loading && local.is_some() && !self.local.valid[index]);
        let summary = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_x_3()
                    .gap_y_1()
                    .child(SOURCES[index].0)
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(if status_error {
                                palette.error
                            } else {
                                palette.text_muted
                            }))
                            .child(self.row_status(index).to_owned()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x_4()
                    .gap_y_1()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child(div().child(format!("Local: {local_summary}")))
                    .child(div().child(format!("S3: {remote_summary}"))),
            );
        let mut content = div()
            .flex()
            .flex_col()
            .min_w_0()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(
                control(
                    ("backup-details", index),
                    "",
                    palette,
                    true,
                    cx,
                    move |view, _, cx| {
                        view.rows[index].expanded = !view.rows[index].expanded;
                        cx.notify();
                    },
                )
                .w_full()
                .h_auto()
                .min_w_0()
                .px_2()
                .py_2()
                .gap_2()
                .justify_start()
                .bg(gpui::transparent_black())
                .border_color(gpui::transparent_black())
                .child(summary)
                .child(
                    Icon::ChevronDown
                        .render(palette.icon_muted)
                        .with_transformation(gpui::Transformation::rotate(gpui::radians(
                            if row.expanded {
                                0.
                            } else {
                                -std::f32::consts::FRAC_PI_2
                            },
                        ))),
                ),
            );
        if let Some(error) = &row.error {
            content = content.child(
                div()
                    .px_2()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(error.clone()),
            );
        }
        if let Some(error) = local.and_then(|record| record.verification_error.as_ref()) {
            content = content.child(
                div()
                    .px_2()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(format!("Local verification: {error}")),
            );
        }
        if row.expanded {
            let mut details = div()
                .flex()
                .flex_col()
                .min_w_0()
                .px_2()
                .pb_1()
                .gap_1()
                .text_sm()
                .text_color(rgb(palette.text_muted))
                .child(detail(
                    index,
                    "Source",
                    format!(
                        "{}/{}/{}",
                        self.config.endpoint.trim_end_matches('/'),
                        self.config.bucket,
                        SOURCES[index].1
                    ),
                ));
            if let Some(remote) = &row.remote {
                details = details
                    .child(detail(index, "S3 file", remote.file_name().to_owned()))
                    .child(detail(
                        index,
                        "S3 modified",
                        exact_date(&remote.modified.to_string()),
                    ));
            }
            if let Some(record) = local {
                details = details
                    .child(detail(index, "Local source file", record.key.clone()))
                    .child(detail(
                        index,
                        "Local modified",
                        exact_date(&record.modified),
                    ));
                if let Some(pair) = &self.local.pair
                    && let Some(cache) = &self.cache
                    && let Ok(directory) = cache.directory(&pair.generation)
                {
                    details = details.child(detail(
                        index,
                        "Local path",
                        directory
                            .join(backup_cache::FILES[index])
                            .display()
                            .to_string(),
                    ));
                }
            }
            content = content.child(details);
        }
        let progress = match row.transfer {
            Transfer::Running { bytes, speed, .. } => Some((
                bytes,
                row.remote.as_ref().map_or(0, |backup| backup.size),
                Some(speed),
            )),
            Transfer::Verifying { bytes, total } => Some((bytes, total, None)),
            _ => None,
        };
        if let Some((bytes, total, speed)) = progress {
            // Compressed input may be buffered ahead of decompression; verification
            // reaches 100% only after structural checks and the digest succeed.
            let fraction = if total == 0 {
                0.
            } else {
                (bytes as f64 / total as f64).clamp(0., if speed.is_some() { 1. } else { 0.99 })
            };
            content = content
                .child(
                    div()
                        .h(px(2.))
                        .w_full()
                        .bg(rgb(palette.element_background))
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(fraction as f32))
                                .bg(rgb(palette.icon)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .flex_wrap()
                        .gap_2()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child(progress_value(44., format!("{:.0}%", fraction * 100.)))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .flex_shrink_0()
                                .child(progress_value(80., format_size(bytes)))
                                .child("/")
                                .child(progress_value(80., format_size(total))),
                        )
                        .children(speed.map(|speed| {
                            progress_value(104., format!("{}/s", format_size(speed)))
                        })),
                );
        }
        content
    }
}
fn progress_value(width: f32, text: String) -> Div {
    let mut value = div()
        .w(px(width))
        .flex_shrink_0()
        .text_right()
        .overflow_hidden()
        .text_ellipsis()
        .child(text);
    value
        .text_style()
        .get_or_insert_with(Default::default)
        .font_features = Some(gpui::FontFeatures(std::sync::Arc::new(vec![(
        "tnum".into(),
        1,
    )])));
    value
}

fn control(
    id: impl Into<gpui::ElementId>,
    label: &str,
    palette: Palette,
    enabled: bool,
    cx: &Context<BackupsView>,
    action: impl Fn(&mut BackupsView, &mut Window, &mut Context<BackupsView>) + 'static,
) -> gpui::Stateful<Div> {
    let action = std::rc::Rc::new(action);
    let key_action = action.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .h(px(28.0))
        .flex_shrink_0()
        .rounded_sm()
        .border_1()
        .border_color(rgb(palette.border))
        .text_sm()
        .bg(rgb(palette.element_background))
        .text_color(rgb(if enabled {
            palette.text
        } else {
            palette.text_muted
        }))
        .when(enabled, |button| {
            button
                .tab_index(0)
                .cursor_pointer()
                .hover(|button| button.bg(rgb(palette.element_hover)))
                .focus(|button| button.border_color(rgb(palette.border_focused)))
        })
        .on_click(cx.listener(move |view, _, window, cx| {
            if enabled {
                action(view, window, cx);
            }
        }))
        .on_key_down(
            cx.listener(move |view, event: &gpui::KeyDownEvent, window, cx| {
                if enabled && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    key_action(view, window, cx);
                    cx.stop_propagation();
                }
            }),
        )
        .child(label.to_owned())
}

impl Render for BackupsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.theme.palette(window.appearance());
        let label = if self.cancel.is_some() {
            if self.publishing {
                "Finishing…"
            } else if self.cancelling {
                "Cancelling…"
            } else {
                "Cancel"
            }
        } else if self.local.missing() {
            "Download"
        } else if self.current() {
            "Up to date"
        } else {
            "Update"
        };
        let enabled = if self.cancel.is_some() {
            !self.publishing && !self.cancelling
        } else {
            self.actionable()
        };
        div()
            .id("backups-content")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .p_4()
            .text_color(rgb(palette.text))
            .bg(rgb(palette.editor_background))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .pb_2()
                    .child(div().text_lg().child("Backups"))
                    .child(div().flex_1())
                    .child(control(
                        "verify-backups",
                        "Verify integrity",
                        palette,
                        !self.busy() && self.cache.is_some() && self.local.pair.is_some(),
                        cx,
                        |view, _, cx| view.verify_integrity(cx),
                    ))
                    .child(
                        control(
                            "check-backups",
                            "",
                            palette,
                            !self.busy(),
                            cx,
                            |view, _, cx| view.check(cx),
                        )
                        .w(px(28.))
                        .h(px(28.))
                        .p_0()
                        .bg(gpui::transparent_black())
                        .border_color(gpui::transparent_black())
                        .child(Icon::Refresh.render(if self.busy() {
                            palette.icon_muted
                        } else {
                            palette.icon
                        }))
                        .tooltip(move |_, cx| cx.new(|_| RefreshTooltip(palette)).into()),
                    )
                    .child(control(
                        "download-backups",
                        label,
                        palette,
                        enabled,
                        cx,
                        |view, _, cx| {
                            if view.cancel.is_some() {
                                view.cancel(cx);
                            } else {
                                view.download(cx);
                            }
                        },
                    )),
            )
            .children(
                self.message
                    .as_ref()
                    .map(|message| div().py_2().text_sm().child(message.clone())),
            )
            .children(self.local_error.as_ref().map(|error| {
                div()
                    .py_2()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(error.clone())
            }))
            .children(self.local.warnings.iter().map(|warning| {
                div()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(warning.clone())
            }))
            .children((0..2).map(|index| self.row(index, palette, cx)))
    }
}

fn compact_metadata(timestamp: &str, size: u64) -> String {
    let date = chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%d.%m.%Y")
                .to_string()
        })
        .unwrap_or_else(|_| "Unknown date".into());
    format!("{date} · {}", format_size(size))
}

fn exact_date(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%d.%m.%Y %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.into())
}

fn detail(index: usize, label: &'static str, value: String) -> gpui::Stateful<Div> {
    // Keep long object keys and paths available without widening the page.
    div()
        .id((label, index))
        .w_full()
        .min_w_0()
        .overflow_x_scroll()
        .whitespace_nowrap()
        .child(format!("{label}: {value}"))
}

struct RefreshTooltip(Palette);
impl Render for RefreshTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .text_xs()
            .rounded_sm()
            .bg(rgb(self.0.panel_background))
            .text_color(rgb(self.0.text))
            .child("Check for backup updates")
    }
}
