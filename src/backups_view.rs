use crate::{
    backup_cache::{self, Cache, Progress, SOURCES, Snapshot},
    backups::{self, Backup, DownloadError, StorageConfig, format_size},
    theme::{Palette, ThemeMode},
};
use gpui::{Context, Div, Render, Window, div, prelude::*, px, rgb};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

enum Transfer {
    Idle,
    Running {
        bytes: u64,
        speed: u64,
        started: Instant,
    },
    Verifying,
    Verified,
}
struct Row {
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
}
pub struct BackupsView {
    config: StorageConfig,
    runtime: Handle,
    rows: [Row; 2],
    events: mpsc::Sender<Event>,
    theme: ThemeMode,
    cache: Option<Cache>,
    local: Snapshot,
    local_loading: bool,
    local_error: Option<String>,
    cancel: Option<backup_cache::UpdateControl>,
    cancelling: bool,
    publishing: bool,
    message: Option<String>,
}
impl BackupsView {
    pub fn new(runtime: Handle, theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        let (events, receiver) = mpsc::channel();
        cx.spawn(async move |view, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(100)).await;
                let pending: Vec<_> = receiver.try_iter().collect();
                if view
                    .update(cx, |view, cx| {
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
        let cache = Cache::application();
        let local_error = cache.as_ref().err().map(|error| format!("{error:#}"));
        let mut view = Self {
            config: crate::database::load_storage()
                .ok()
                .flatten()
                .unwrap_or_default(),
            runtime,
            theme,
            events,
            rows: std::array::from_fn(|_| Row {
                loading: false,
                remote: None,
                error: None,
                transfer: Transfer::Idle,
            }),
            cache: cache.ok(),
            local: Snapshot::default(),
            local_loading: false,
            local_error,
            cancel: None,
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
    pub fn configure(
        &mut self,
        config: StorageConfig,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("Wait for the current operation before changing the connection.".into());
        }
        if config.access_key.is_empty()
            || config.secret_key.is_empty()
            || config.bucket.is_empty()
            || config.region.is_empty()
            || !(config.endpoint.starts_with("https://") || config.endpoint.starts_with("http://"))
        {
            return Err("Enter an HTTP(S) endpoint, bucket, region and both keys.".into());
        }
        crate::database::save_storage(&config)
            .map_err(|error| format!("Could not save connection: {error}"))?;
        self.config = config;
        self.check(cx);
        Ok(())
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
        for (index, (_, prefix)) in SOURCES.into_iter().enumerate() {
            self.rows[index].loading = true;
            self.rows[index].error = None;
            let config = self.config.clone();
            let events = self.events.clone();
            self.runtime.spawn(async move {
                let result = async {
                    let client = config.client().await?;
                    backups::latest_backup(&client, &config.bucket, prefix).await
                }
                .await
                .map_err(|error: anyhow::Error| format!("{error:#}"));
                let _ = events.send(Event::Listed(index, result));
            });
        }
    }
    fn check(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.message = None;
        if let Some(cache) = self.cache.clone() {
            self.local_loading = true;
            let events = self.events.clone();
            self.runtime.spawn(async move {
                let result = tokio::task::spawn_blocking(move || cache.load())
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result.map_err(|error| format!("{error:#}")));
                let _ = events.send(Event::Local(result));
            });
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
        let Some(cache) = self.cache.clone() else {
            return;
        };
        let control = backup_cache::UpdateControl::new();
        self.cancel = Some(control.clone());
        self.cancelling = false;
        self.publishing = false;
        self.message = None;
        for row in &mut self.rows {
            row.transfer = Transfer::Idle;
        }
        let events = self.events.clone();
        let config = self.config.clone();
        self.runtime.spawn(async move {
            let result = backup_cache::update(cache, config, remote, control, |progress| {
                let _ = events.send(Event::Progress(progress));
            })
            .await;
            let _ = events.send(Event::Finished(result));
        });
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
                Progress::Verifying(index) => {
                    self.rows[index].transfer = Transfer::Verifying;
                    tracing::info!(target:"app::download",source=SOURCES[index].0,"Verifying backup archive");
                }
                Progress::Verified(index) => self.rows[index].transfer = Transfer::Verified,
                Progress::Publishing => self.publishing = true,
            },
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
    fn row(&self, index: usize, palette: Palette) -> Div {
        let row = &self.rows[index];
        let mut content = div()
            .flex()
            .flex_col()
            .min_w_0()
            .gap_3()
            .py_5()
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(SOURCES[index].0)
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child(format!("{}/{}", self.config.bucket, SOURCES[index].1)),
            );
        if row.loading {
            content = content.child(div().text_sm().child("Checking S3…"));
        }
        if let Some(error) = &row.error {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(error.clone()),
            );
        } else if let Some(remote) = &row.remote {
            content = content
                .child(
                    div()
                        .text_sm()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(remote.file_name().to_owned()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child(format!(
                            "{} · {} UTC",
                            format_size(remote.size),
                            remote.modified
                        )),
                );
        } else if !row.loading {
            content = content.child(div().text_sm().child("No backup found in S3."));
        }
        if self.local_loading {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child("Checking local copy…"),
            );
        } else if self.local.valid[index] {
            if let Some(pair) = &self.local.pair {
                let record = &pair.records[index];
                let status = if row.loading || row.error.is_some() {
                    "Local copy available; freshness unknown"
                } else if pair.origin == backup_cache::Origin::new(&self.config)
                    && row
                        .remote
                        .as_ref()
                        .is_some_and(|remote| record.matches(remote))
                {
                    "Local copy is up to date"
                } else {
                    "Update available"
                };
                content = content.child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child(format!("{status} · {}", record.key)),
                );
            }
        } else {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child("Local copy is missing or failed verification."),
            );
        }
        match &row.transfer {
            Transfer::Running { bytes, speed, .. } => {
                let total = row.remote.as_ref().map_or(0, |backup| backup.size);
                let fraction = if total == 0 {
                    0.
                } else {
                    (*bytes as f64 / total as f64).clamp(0., 1.)
                };
                content = content
                    .child(
                        div()
                            .h(px(4.))
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
                            .child(progress_value(48., format!("{:.0}%", fraction * 100.)))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .flex_shrink_0()
                                    .child(progress_value(100., format_size(*bytes)))
                                    .child("/")
                                    .child(progress_value(100., format_size(total))),
                            )
                            .child(progress_value(120., format!("{}/s", format_size(*speed)))),
                    );
            }
            Transfer::Verifying => {
                content = content.child(div().text_sm().child("Verifying archive integrity…"))
            }
            Transfer::Verified => {
                content = content.child(
                    div()
                        .text_sm()
                        .child("New file verified; waiting for the complete pair."),
                )
            }
            Transfer::Idle => {}
        }
        content
    }
}
impl Drop for BackupsView {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            let _ = cancel.cancel();
        }
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
        .h(px(32.0))
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
        div().id("backups-content").flex().flex_col().flex_1().min_h_0().min_w_0().overflow_y_scroll().p_6().text_color(rgb(palette.text)).bg(rgb(palette.editor_background))
            .child(div().flex().items_center().flex_wrap().gap_2().pb_2().child(div().text_lg().child("Latest backups")).child(div().flex_1())
                .child(control("check-backups", "Refresh", palette, !self.busy(), cx, |view, _, cx| view.check(cx)))
                .child(control("download-backups", label, palette, enabled, cx, |view, _, cx| { if view.cancel.is_some() { view.cancel(cx); } else { view.download(cx); } })))
            .child(div().text_sm().text_color(rgb(palette.text_muted)).child("One local copy per database. Download and update always replace both backups together."))
            .children(self.message.as_ref().map(|message| div().py_2().text_sm().child(message.clone())))
            .children(self.local_error.as_ref().map(|error| div().py_2().text_sm().text_color(rgb(palette.error)).child(error.clone())))
            .children(self.local.warnings.iter().map(|warning| div().text_sm().text_color(rgb(palette.error)).child(warning.clone())))
            .children((0..2).map(|index| self.row(index, palette)))
    }
}
