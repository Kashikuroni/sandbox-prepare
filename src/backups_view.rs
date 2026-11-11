use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use gpui::{
    Context, Div, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, prelude::*, px, rgb,
};
use tokio::{runtime::Handle, sync::watch};

use crate::{
    backups::{self, Backup, DownloadError, StorageConfig, format_size},
    theme::{Palette, ThemeMode},
};

const SOURCES: [(&str, &str); 2] = [("ClickHouse", "ch/"), ("MySQL", "mysql/")];

enum DownloadState {
    Idle,
    Preparing,
    Choosing,
    Running {
        bytes: u64,
        speed: u64,
        started: Instant,
        cancel: watch::Sender<bool>,
    },
    Cancelling,
    Cancelled,
    Complete(PathBuf),
    Failed(String),
}

struct BackupRow {
    loading: bool,
    backup: Option<Backup>,
    error: Option<String>,
    download: DownloadState,
}

impl BackupRow {
    fn busy(&self) -> bool {
        matches!(
            self.download,
            DownloadState::Preparing
                | DownloadState::Choosing
                | DownloadState::Running { .. }
                | DownloadState::Cancelling
        )
    }
}

enum Event {
    Listed(usize, Result<Option<Backup>, String>),
    Prepared(usize, Backup, Result<PathBuf, String>),
    Progress(usize, u64),
    Finished(usize, Result<PathBuf, DownloadError>),
}

pub struct BackupsView {
    config: StorageConfig,
    runtime: Handle,
    rows: [BackupRow; 2],
    events: mpsc::Sender<Event>,
    theme: ThemeMode,
    download_directory: Option<PathBuf>,
}

impl BackupsView {
    pub fn new(
        runtime: Handle,
        theme: ThemeMode,
        download_directory: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (events, receiver) = mpsc::channel();
        // Poll only the small, throttled UI event queue. All S3 and disk I/O runs on Tokio.
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
                            view.handle_event(event, cx);
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
        let mut view = Self {
            config: crate::database::load_storage()
                .ok()
                .flatten()
                .unwrap_or_default(),
            runtime,
            rows: std::array::from_fn(|_| BackupRow {
                loading: false,
                backup: None,
                error: None,
                download: DownloadState::Idle,
            }),
            events,
            theme,
            download_directory,
        };
        view.refresh(cx);
        view
    }

    pub fn configure(
        &mut self,
        config: StorageConfig,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.rows.iter().any(|row| row.loading || row.busy()) {
            return Err(
                "Wait for the current operation to finish before changing the connection.".into(),
            );
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
        self.refresh(cx);
        Ok(())
    }

    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    pub fn set_download_directory(&mut self, directory: Option<PathBuf>) {
        self.download_directory = directory;
    }

    fn handle_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Prepared(index, backup, result) => match result {
                Ok(path) => self.choose_destination(index, backup, path, cx),
                Err(error) => {
                    tracing::error!(target:"app::download",source=SOURCES[index].0,%error,"Could not prepare download");
                    self.rows[index].download = DownloadState::Failed(error);
                }
            },
            Event::Listed(index, result) => {
                let row = &mut self.rows[index];
                row.loading = false;
                match result {
                    Ok(backup) => {
                        tracing::info!(target:"app::s3",source=SOURCES[index].0,found=backup.is_some(),"Latest backup lookup completed");
                        row.backup = backup;
                    }
                    Err(error) => {
                        tracing::error!(target:"app::s3",source=SOURCES[index].0,%error,"Backup lookup failed");
                        row.error = Some(error);
                    }
                }
            }
            Event::Progress(index, bytes) => {
                if let DownloadState::Running {
                    bytes: downloaded,
                    speed,
                    started,
                    ..
                } = &mut self.rows[index].download
                {
                    *downloaded = bytes;
                    *speed = (bytes as f64 / started.elapsed().as_secs_f64().max(0.1)) as u64;
                }
            }
            Event::Finished(index, result) => {
                self.rows[index].download = match result {
                    Ok(path) => {
                        tracing::info!(target:"app::download",source=SOURCES[index].0,path=%path.display(),"Backup saved");
                        DownloadState::Complete(path)
                    }
                    Err(DownloadError::Cancelled) => {
                        tracing::info!(target:"app::download",source=SOURCES[index].0,"Download cancelled");
                        DownloadState::Cancelled
                    }
                    Err(DownloadError::Failed(error)) => {
                        tracing::error!(target:"app::download",source=SOURCES[index].0,%error,"Download failed");
                        DownloadState::Failed(error)
                    }
                };
            }
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.rows.iter().any(|row| row.loading || row.busy()) {
            return;
        }
        for (index, (_, prefix)) in SOURCES.into_iter().enumerate() {
            self.rows[index] = BackupRow {
                loading: true,
                backup: None,
                error: None,
                download: DownloadState::Idle,
            };
            let events = self.events.clone();
            let config = self.config.clone();
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
        cx.notify();
    }

    fn download_latest(&mut self, index: usize, cx: &mut Context<Self>) {
        let row = &mut self.rows[index];
        if row.busy() {
            return;
        }
        let Some(backup) = row.backup.clone() else {
            return;
        };
        let Some(directory) = self.download_directory.clone() else {
            row.download =
                DownloadState::Failed("Choose a download folder in Settings → Backups.".into());
            cx.notify();
            return;
        };
        row.download = DownloadState::Preparing;
        let events = self.events.clone();
        self.runtime.spawn(async move {
            let result = backups::prepare_destination(&directory, backup.file_name())
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = events.send(Event::Prepared(index, backup, result));
        });
        cx.notify();
    }

    fn choose_destination(
        &mut self,
        index: usize,
        backup: Backup,
        suggested: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let directory = suggested.parent().unwrap();
        let name = suggested.file_name().and_then(|name| name.to_str());
        let prompt = cx.prompt_for_new_path(directory, name);
        let row = &mut self.rows[index];
        row.download = DownloadState::Choosing;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = prompt.await;
            let _ = view.update(cx, |view, cx| {
                match result {
                    Ok(Ok(Some(path))) => view.start_download(index, backup, path),
                    Ok(Ok(None)) => view.rows[index].download = DownloadState::Idle,
                    Ok(Err(error)) => {
                        view.rows[index].download = DownloadState::Failed(format!(
                            "Could not open the save dialog: {error}"
                        ))
                    }
                    Err(_) => view.rows[index].download = DownloadState::Idle,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_download(&mut self, index: usize, backup: Backup, path: PathBuf) {
        tracing::info!(target:"app::download",source=SOURCES[index].0,key=%backup.key,bytes=backup.size,"Download started");
        let (cancel, receiver) = watch::channel(false);
        self.rows[index].download = DownloadState::Running {
            bytes: 0,
            speed: 0,
            started: Instant::now(),
            cancel,
        };
        let config = self.config.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            let result = match config.client().await {
                Ok(client) => {
                    backups::download(&client, &config.bucket, &backup, &path, receiver, |bytes| {
                        let _ = events.send(Event::Progress(index, bytes));
                    })
                    .await
                }
                Err(error) => Err(DownloadError::Failed(format!("{error:#}"))),
            };
            let _ = events.send(Event::Finished(index, result));
        });
    }

    fn cancel(&mut self, index: usize, cx: &mut Context<Self>) {
        if let DownloadState::Running { cancel, .. } = &self.rows[index].download {
            let _ = cancel.send(true);
            self.rows[index].download = DownloadState::Cancelling;
            tracing::debug!(target:"app::download",source=SOURCES[index].0,"Cancellation requested");
            cx.notify();
        }
    }

    fn render_row(&self, index: usize, palette: Palette, cx: &mut Context<Self>) -> Div {
        let (label, prefix) = SOURCES[index];
        let row = &self.rows[index];
        let mut content = div()
            .flex()
            .flex_col()
            .gap_3()
            .py_5()
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div().flex().flex_col().gap_1().child(label).child(
                            div()
                                .text_sm()
                                .text_color(rgb(palette.text_muted))
                                .child(format!("{}/{prefix}", self.config.bucket)),
                        ),
                    )
                    .when(row.backup.is_some() && !row.loading, |header| {
                        header.child(control(
                            ("download", index),
                            if row.busy() {
                                "Downloading…"
                            } else {
                                "Download latest"
                            },
                            palette,
                            !row.busy(),
                            cx,
                            move |view, _, cx| view.download_latest(index, cx),
                        ))
                    }),
            );
        if row.loading {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child("Looking for the latest backup…"),
            );
        } else if let Some(error) = &row.error {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(error.clone()),
            );
        } else if let Some(backup) = &row.backup {
            content = content
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(backup.file_name().to_owned()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child(format!(
                            "{}  ·  {} UTC",
                            format_size(backup.size),
                            backup.modified
                        )),
                );
        } else {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child("No backup files found in this folder."),
            );
        }
        match &row.download {
            DownloadState::Running { bytes, speed, .. } => {
                let total = row.backup.as_ref().map_or(0, |backup| backup.size);
                let fraction = if total == 0 {
                    0.0
                } else {
                    (*bytes as f64 / total as f64).clamp(0.0, 1.0)
                };
                content = content
                    .child(
                        div()
                            .h(px(4.0))
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
                            .child(progress_value(120., format!("{}/s", format_size(*speed))))
                            .child(div().flex_1())
                            .child(control(
                                ("cancel", index),
                                "Cancel",
                                palette,
                                true,
                                cx,
                                move |view, _, cx| view.cancel(index, cx),
                            )),
                    );
            }
            DownloadState::Preparing => {
                content = content.child(div().text_sm().child("Preparing download…"));
            }
            DownloadState::Choosing => {
                content = content.child(div().text_sm().child("Choose where to save the backup…"))
            }
            DownloadState::Cancelling => {
                content = content.child(div().text_sm().child("Cancelling download…"))
            }
            DownloadState::Cancelled => {
                content = content.child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child("Download cancelled."),
                )
            }
            DownloadState::Failed(error) => {
                content = content.child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.error))
                        .child(error.clone()),
                )
            }
            DownloadState::Complete(path) => {
                let reveal = path.clone();
                content = content
                    .child(
                        div()
                            .text_sm()
                            .child(format!("Saved to {}", path.display())),
                    )
                    .child(control(
                        ("reveal", index),
                        "Show in folder",
                        palette,
                        true,
                        cx,
                        move |_, _, cx| cx.reveal_path(&reveal),
                    ));
            }
            DownloadState::Idle => {}
        }
        content
    }
}

impl Drop for BackupsView {
    fn drop(&mut self) {
        for row in &self.rows {
            if let DownloadState::Running { cancel, .. } = &row.download {
                let _ = cancel.send(true);
            }
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
        let can_refresh = !self.rows.iter().any(|row| row.loading || row.busy());
        div()
            .id("backups-content")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .p_6()
            .text_color(rgb(palette.text))
            .bg(rgb(palette.editor_background))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .pb_2()
                    .child(div().text_lg().child("Latest backups"))
                    .child(control(
                        "refresh-backups",
                        if self.rows.iter().any(|row| row.loading) {
                            "Checking…"
                        } else {
                            "Refresh"
                        },
                        palette,
                        can_refresh,
                        cx,
                        |view, _, cx| view.refresh(cx),
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child("The newest file in each folder, selected by last modified time."),
            )
            .children((0..SOURCES.len()).map(|index| self.render_row(index, palette, cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt_4()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child("Connection managed in Settings → Storage")
                    .child(self.config.endpoint.clone()),
            )
    }
}
