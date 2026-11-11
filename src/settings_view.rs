use crate::{
    backups::StorageConfig,
    backups_view::BackupsView,
    logging,
    settings::{Settings, SettingsModel},
    text_input::{Edited, TextInput},
    theme::{Palette, ThemeMode},
};
use gpui::{Context, Div, Entity, FocusHandle, Render, Stateful, Window, div, prelude::*, px, rgb};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Appearance,
    Storage,
    Logging,
}
impl Section {
    const ALL: [Self; 3] = [Self::Appearance, Self::Storage, Self::Logging];
    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Storage => "Storage",
            Self::Logging => "Logging",
        }
    }
}
#[derive(Clone, Copy)]
enum Destination {
    Close,
    Form(Section),
    Json,
}
#[derive(Clone, Copy)]
enum Confirmation {
    Clear,
    Discard(Destination),
}

pub struct SettingsWindow {
    model: Entity<SettingsModel>,
    backups: Entity<BackupsView>,
    section: Section,
    search: Entity<TextInput>,
    storage: [Entity<TextInput>; 5],
    saved_storage: [String; 5],
    logs: [Entity<TextInput>; 4],
    editor: Entity<TextInput>,
    json_mode: bool,
    message: Option<(bool, String)>,
    focus: FocusHandle,
    modal: Option<Confirmation>,
    modal_focus: [FocusHandle; 2],
    busy: bool,
    log_save_tasks: [Option<gpui::Task<()>>; 4],
    log_errors: [Option<String>; 4],
}
impl SettingsWindow {
    pub fn new(
        model: Entity<SettingsModel>,
        backups: Entity<BackupsView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = model.read(cx).value.clone();
        let theme = settings.theme;
        let search = cx.new(|cx| TextInput::search(theme, cx));
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
        let config = crate::database::load_storage();
        let message = config
            .as_ref()
            .err()
            .map(|error| (true, format!("Could not load connection: {error:#}")));
        let config = config.ok().flatten().unwrap_or_default();
        let saved_storage = [
            config.endpoint,
            config.bucket,
            config.region,
            config.access_key,
            config.secret_key,
        ];
        let storage = std::array::from_fn(|index| {
            cx.new(|cx| TextInput::new(saved_storage[index].clone(), index >= 3, theme, cx))
        });
        let values = log_values(&settings);
        let logs = std::array::from_fn(|index| {
            cx.new(|cx| TextInput::new(values[index].clone(), false, theme, cx))
        });
        for (index, field) in logs.iter().enumerate() {
            cx.subscribe(field, move |view, _, _: &Edited, cx| {
                view.schedule_log_save(index, cx)
            })
            .detach();
        }
        let editor =
            cx.new(|cx| TextInput::multiline(settings.json().unwrap_or_default(), theme, cx));
        let focus = cx.focus_handle();
        search.read(cx).focus(window);
        cx.observe(&model, |view, model, cx| {
            view.set_input_theme(model.read(cx).value.theme, cx);
            cx.notify();
        })
        .detach();
        cx.observe_window_appearance(window, |view, _, cx| {
            view.set_input_theme(view.model.read(cx).value.theme, cx);
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |_, _, cx| cx.notify())
            .detach();
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |view, cx| {
                view.flush_logging(cx);
                if view.has_unsaved(cx) {
                    view.confirm(Confirmation::Discard(Destination::Close), window, cx);
                    false
                } else {
                    true
                }
            })
            .unwrap_or(true)
        });
        // Refresh the last automatic check status even while no logs are arriving.
        cx.spawn(async move |view, cx| {
            loop {
                gpui::Timer::after(Duration::from_secs(30)).await;
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            model,
            backups,
            section: Section::Appearance,
            search,
            storage,
            saved_storage,
            logs,
            editor,
            json_mode: false,
            message,
            focus,
            modal: None,
            modal_focus: std::array::from_fn(|_| cx.focus_handle()),
            busy: false,
            log_save_tasks: std::array::from_fn(|_| None),
            log_errors: std::array::from_fn(|_| None),
        }
    }
    fn set_input_theme(&self, theme: ThemeMode, cx: &mut Context<Self>) {
        for field in self
            .storage
            .iter()
            .chain(self.logs.iter())
            .chain([&self.search, &self.editor])
        {
            field.update(cx, |field, cx| field.set_theme(theme, cx));
        }
    }
    fn move_focus(&self, previous: bool, window: &mut Window) {
        if self.modal.is_some() {
            let index = usize::from(self.modal_focus[0].is_focused(window));
            self.modal_focus[index].focus(window);
        } else if previous {
            window.focus_prev();
        } else {
            window.focus_next();
        }
    }
    fn has_unsaved(&self, cx: &gpui::App) -> bool {
        let settings = &self.model.read(cx).value;
        (self.json_mode && self.editor.read(cx).value() != settings.json().unwrap_or_default())
            || self.logs.iter().enumerate().any(|(index, field)| {
                !logging_edit(settings, index, &field.read(cx).value())
                    .is_ok_and(|edited| edited == *settings)
            })
            || self
                .storage
                .iter()
                .zip(&self.saved_storage)
                .any(|(field, value)| field.read(cx).value() != *value)
    }
    fn sync_fields(&self, cx: &mut Context<Self>) {
        let settings = self.model.read(cx).value.clone();
        for (field, value) in self.logs.iter().zip(log_values(&settings)) {
            field.update(cx, |field, cx| field.set_value(value, cx));
        }
        self.editor.update(cx, |field, cx| {
            field.set_value(settings.json().unwrap_or_default(), cx)
        });
    }
    fn confirm(&mut self, confirmation: Confirmation, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = Some(confirmation);
        self.modal_focus[0].focus(window);
        cx.notify();
    }
    fn request(&mut self, destination: Destination, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_logging(cx);
        if self.has_unsaved(cx) {
            self.confirm(Confirmation::Discard(destination), window, cx);
        } else {
            self.navigate(destination, window, cx);
        }
    }
    fn navigate(&mut self, destination: Destination, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        match destination {
            Destination::Close => window.remove_window(),
            Destination::Form(section) => {
                self.section = section;
                self.json_mode = false;
                self.focus.focus(window);
            }
            Destination::Json => {
                self.json_mode = true;
                self.editor.update(cx, |field, cx| {
                    field.set_value(self.model.read(cx).value.json().unwrap_or_default(), cx)
                });
                self.editor.read(cx).focus(window);
            }
        }
        self.message = None;
        cx.notify();
    }
    fn discard(&mut self, destination: Destination, window: &mut Window, cx: &mut Context<Self>) {
        self.log_save_tasks = std::array::from_fn(|_| None);
        self.log_errors = std::array::from_fn(|_| None);
        self.sync_fields(cx);
        for (field, value) in self.storage.iter().zip(&self.saved_storage) {
            field.update(cx, |field, cx| field.set_value(value.clone(), cx));
        }
        self.navigate(destination, window, cx);
    }
    fn save(&mut self, settings: Settings, cx: &mut Context<Self>) -> bool {
        match self.model.update(cx, |model, cx| model.save(settings, cx)) {
            Ok(()) => {
                self.message = Some((false, "Saved. Changes are active.".into()));
                true
            }
            Err(error) => {
                self.message = Some((true, format!("Settings update failed: {error:#}")));
                false
            }
        }
    }
    fn save_json(&mut self, cx: &mut Context<Self>) {
        match Settings::from_json(&self.editor.read(cx).value()) {
            Ok(settings) => {
                if self.save(settings, cx) {
                    self.sync_fields(cx);
                }
            }
            Err(error) => self.message = Some((true, format!("{error:#}"))),
        }
        cx.notify();
    }
    fn schedule_log_save(&mut self, index: usize, cx: &mut Context<Self>) {
        self.log_errors[index] = None;
        self.log_save_tasks[index] = Some(cx.spawn(async move |view, cx| {
            gpui::Timer::after(Duration::from_millis(300)).await;
            let _ = view.update(cx, |view, cx| view.apply_log_edit(index, cx));
        }));
        cx.notify();
    }
    fn flush_logging(&mut self, cx: &mut Context<Self>) {
        self.log_save_tasks = std::array::from_fn(|_| None);
        if !self.json_mode {
            for index in 0..self.logs.len() {
                self.apply_log_edit(index, cx);
            }
        }
    }
    fn apply_log_edit(&mut self, index: usize, cx: &mut Context<Self>) {
        let current = &self.model.read(cx).value;
        let result = logging_edit(current, index, &self.logs[index].read(cx).value());
        self.log_errors[index] = match result {
            Ok(settings) if settings == *current => None,
            Ok(settings) => self
                .model
                .update(cx, |model, cx| model.save(settings, cx))
                .err()
                .map(|error| format!("{error:#}")),
            Err(error) => Some(error.to_string()),
        };
        // Do not replace input values: edits must preserve selection, caret, and focus.
        cx.notify();
    }
    fn save_storage(&mut self, cx: &mut Context<Self>) {
        let values: [String; 5] = std::array::from_fn(|index| self.storage[index].read(cx).value());
        let config = StorageConfig {
            endpoint: values[0].trim().into(),
            bucket: values[1].trim().into(),
            region: values[2].trim().into(),
            access_key: values[3].clone(),
            secret_key: values[4].clone(),
        };
        self.message = Some(
            match self
                .backups
                .update(cx, |view, cx| view.configure(config, cx))
            {
                Ok(()) => {
                    self.saved_storage = values;
                    (false, "Connection saved. Checking backups…".into())
                }
                Err(error) => (true, error),
            },
        );
        cx.notify();
    }
    fn maintenance(&mut self, clear: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if !clear {
            self.flush_logging(cx);
            if self.log_errors.iter().any(Option::is_some) {
                self.message = Some((
                    true,
                    "Correct logging settings before checking retention.".into(),
                ));
                cx.notify();
                return;
            }
        }
        self.modal = None;
        self.focus.focus(window);
        self.busy = true;
        self.message = Some((
            false,
            if clear {
                "Clearing logs…"
            } else {
                "Checking log retention…"
            }
            .into(),
        ));
        let (sender, receiver) = std::sync::mpsc::channel();
        let store = logging::store();
        std::thread::spawn(move || {
            let _ = sender.send(store.maintain(clear));
        });
        cx.spawn(async move |view, cx| loop {
            gpui::Timer::after(Duration::from_millis(100)).await;
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => { if view.upgrade().is_none() { break; } continue; }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("Log maintenance worker stopped")),
            };
            {
                let _ = view.update(cx, |view, cx| {
                    view.busy = false;
                    view.message = Some(match result {
                        Ok(report) => {
                            let message = format!("Deleted {} files. Freed {}.{}", report.deleted_files, crate::backups::format_size(report.freed_bytes), if report.errors.is_empty() { String::new() } else { format!(" {}", report.errors.join("; ")) });
                            tracing::info!(target: "app::storage", deleted_files=report.deleted_files, freed_bytes=report.freed_bytes, clear, "Log maintenance completed");
                            (!report.errors.is_empty(), message)
                        }
                        Err(error) => (true, format!("Log maintenance failed: {error:#}")),
                    });
                    cx.notify();
                });
                break;
            }
        }).detach();
        cx.notify();
    }
    fn matches(&self, title: &str, description: &str, section: Section, cx: &gpui::App) -> bool {
        let query = self.search.read(cx).value().trim().to_lowercase();
        if query.is_empty() {
            self.section == section
        } else {
            format!("{} {title} {description}", section.label())
                .to_lowercase()
                .contains(&query)
        }
    }
    fn render_form(&self, palette: Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut rows = Vec::new();
        if self.matches(
            "Theme",
            "Light Dark System appearance",
            Section::Appearance,
            cx,
        ) {
            rows.push(
                setting_row(
                    "Theme",
                    "Choose Ayu Light, Ayu Mirage, or follow the system appearance.",
                    palette,
                )
                .child(div().flex().flex_shrink_0().gap_1().children(
                    ThemeMode::ALL.into_iter().map(|mode| {
                        button(
                            mode.label(),
                            mode.label(),
                            palette,
                            cx,
                            move |view, _, cx| {
                                let mut value = view.model.read(cx).value.clone();
                                value.theme = mode;
                                view.save(value, cx);
                                cx.notify();
                            },
                        )
                        .when(self.model.read(cx).value.theme == mode, |el| {
                            el.bg(rgb(palette.element_selected))
                        })
                    }),
                )),
            );
        }
        let mut storage_visible = false;
        for (index, (title, description)) in [
            ("Endpoint", "S3-compatible service URL."),
            ("Bucket", "Bucket containing ch/ and mysql/ backups."),
            ("Region", "Region used to sign S3 requests."),
            ("Access key", "Saved locally on this computer."),
            ("Secret key", "Saved locally on this computer."),
        ]
        .into_iter()
        .enumerate()
        {
            if self.matches(title, description, Section::Storage, cx) {
                storage_visible = true;
                rows.push(
                    setting_row(title, description, palette).child(
                        div()
                            .w(px(280.))
                            .flex_shrink_0()
                            .child(self.storage[index].clone()),
                    ),
                );
            }
        }
        if storage_visible {
            rows.push(div().flex().justify_end().py_4().child(button(
                "save-storage",
                "Save connection",
                palette,
                cx,
                |view, _, cx| view.save_storage(cx),
            )));
        }
        for (index, (title, description)) in [
            (
                "Current log records",
                "Complete records kept in memory. Older entries remain in History.",
            ),
            (
                "Maximum file size (MiB)",
                "Start a new file at this size. Complete entries are never split.",
            ),
            (
                "Keep logs for (days)",
                "Delete closed files older than this many days since their last write.",
            ),
            (
                "Check every (hours)",
                "Check retention at this interval and on startup when a check is due.",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if self.matches(title, description, Section::Logging, cx) {
                rows.push(
                    setting_row(title, description, palette).child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w(px(120.))
                            .flex_shrink_0()
                            .child(self.logs[index].clone())
                            .children(self.log_errors[index].as_ref().map(|error| {
                                div()
                                    .text_xs()
                                    .text_color(rgb(palette.error))
                                    .child(error.clone())
                            })),
                    ),
                );
            }
        }
        if self.matches(
            "Log maintenance",
            "check delete clear rotation retention history files",
            Section::Logging,
            cx,
        ) {
            let report = logging::store().maintenance_status();
            let last = chrono::DateTime::from_timestamp(report.checked_at as i64, 0)
                .map(|date| {
                    date.with_timezone(&chrono::Local)
                        .format("%d.%m.%Y %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .py_5()
                    .child(div().text_sm().text_color(rgb(palette.text_muted)).child(
                        if report.checked_at == 0 {
                            "No retention checks yet.".into()
                        } else {
                            format!(
                                "Last check: {last}. Deleted {} files; freed {}.",
                                report.deleted_files,
                                crate::backups::format_size(report.freed_bytes)
                            )
                        },
                    ))
                    .children(report.errors.iter().map(|error| {
                        div()
                            .text_sm()
                            .text_color(rgb(palette.error))
                            .child(error.clone())
                    }))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button(
                                    "check-logs",
                                    if self.busy { "Working…" } else { "Check now" },
                                    palette,
                                    cx,
                                    |view, window, cx| view.maintenance(false, window, cx),
                                )
                                .when(self.busy, |el| el.opacity(0.5)),
                            )
                            .child(
                                button(
                                    "clear-logs",
                                    "Clear all logs…",
                                    palette,
                                    cx,
                                    |view, window, cx| {
                                        if !view.busy {
                                            view.confirm(Confirmation::Clear, window, cx);
                                        }
                                    },
                                )
                                .text_color(rgb(palette.error))
                                .when(self.busy, |el| el.opacity(0.5)),
                            ),
                    ),
            );
        }
        div()
            .id("settings-form")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_6()
            .child(div().text_xl().py_4().child(
                if self.search.read(cx).value().trim().is_empty() {
                    self.section.label()
                } else {
                    "Search results"
                },
            ))
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .py_5()
                        .text_color(rgb(palette.text_muted))
                        .child("No matching settings."),
                )
            })
            .children(rows)
    }
    fn render_modal(&self, palette: Palette, cx: &mut Context<Self>) -> Div {
        let confirmation = self.modal.unwrap();
        let (title, description, action) = match confirmation {
            Confirmation::Clear => (
                "Delete all logs?",
                "All log files and Current records will be permanently deleted. A new log file will be started. This cannot be undone.",
                "Delete all logs",
            ),
            Confirmation::Discard(_) => (
                "Discard unsaved changes?",
                "Your unsaved settings edits will be lost.",
                "Discard changes",
            ),
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.3))
            .occlude()
            .on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => {
                        view.modal = None;
                        view.focus.focus(window);
                        cx.notify();
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                div()
                    .w(px(460.))
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .rounded_md()
                    .bg(rgb(palette.editor_background))
                    .border_1()
                    .border_color(rgb(palette.border))
                    .shadow_lg()
                    .child(div().text_lg().child(title))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(palette.text_muted))
                            .child(description),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                button(
                                    "cancel-confirmation",
                                    "Cancel",
                                    palette,
                                    cx,
                                    |view, window, cx| {
                                        view.modal = None;
                                        view.focus.focus(window);
                                        cx.notify();
                                    },
                                )
                                .track_focus(&self.modal_focus[0]),
                            )
                            .child(
                                button(
                                    "confirm-action",
                                    action,
                                    palette,
                                    cx,
                                    move |view, window, cx| match confirmation {
                                        Confirmation::Clear => view.maintenance(true, window, cx),
                                        Confirmation::Discard(destination) => {
                                            view.discard(destination, window, cx)
                                        }
                                    },
                                )
                                .track_focus(&self.modal_focus[1])
                                .text_color(rgb(palette.error)),
                            ),
                    ),
            )
    }
}
impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.model.read(cx).value.theme.palette(window.appearance());
        div().relative().flex().size_full().key_context("Settings").track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &crate::FocusNext, window, cx| { view.move_focus(false, window); cx.stop_propagation(); }))
            .on_action(cx.listener(|view, _: &crate::FocusPrevious, window, cx| { view.move_focus(true, window); cx.stop_propagation(); }))
            .on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "s" && (event.keystroke.modifiers.platform || event.keystroke.modifiers.control) && view.modal.is_none() {
                    if view.json_mode { view.save_json(cx); } else { match view.section { Section::Storage => view.save_storage(cx), Section::Logging => view.flush_logging(cx), _ => {} } }
                    cx.stop_propagation();
                }
            }))
            .bg(rgb(palette.editor_background)).text_color(rgb(palette.text)).text_base()
            .child(div().flex().flex_col().w(px(240.)).flex_shrink_0().h_full().bg(rgb(palette.panel_background)).border_r_1().border_color(rgb(palette.border))
                .child(div().h(px(40.)).flex_shrink_0().window_control_area(gpui::WindowControlArea::Drag))
                .child(div().px_3().py_2().child(self.search.clone()))
                .children(Section::ALL.into_iter().map(|section| {
                    button(section.label(), section.label(), palette, cx, move |view, window, cx| view.request(Destination::Form(section), window, cx))
                        .mx_3().h(px(34.)).when(!self.json_mode && self.section == section, |el| el.bg(rgb(palette.element_selected)))
                })))
            .child(div().flex().flex_col().flex_1().min_w_0().h_full()
                .child(div().flex().items_center().h(px(48.)).flex_shrink_0().px_6().gap_2()
                    .child(div().flex().items_center().flex_1().h_full().gap_2().window_control_area(gpui::WindowControlArea::Drag)
                        .on_mouse_down(gpui::MouseButton::Left, |event, window, _| { if event.click_count == 2 { window.titlebar_double_click(); } else { window.start_window_move(); } }))
                    .child(if self.json_mode {
                        button("show-form", "Back to settings", palette, cx, |view, window, cx| view.request(Destination::Form(view.section), window, cx))
                    } else { button("edit-settings-json", "Edit settings.json", palette, cx, |view, window, cx| view.request(Destination::Json, window, cx)) }.border_color(rgb(palette.border)).bg(rgb(palette.panel_background))))
                .children(self.model.read(cx).error.as_ref().map(|error| div().px_6().py_2().text_sm().text_color(rgb(palette.error)).child(error.clone())))
                .children(self.message.as_ref().map(|(error, message)| div().px_6().py_2().text_sm().text_color(rgb(if *error { palette.error } else { palette.text_muted })).child(message.clone())))
                .when(self.json_mode, |el| el.child(div().flex().flex_col().flex_1().min_h_0().px_6().pb_6().gap_3()
                    .child(div().text_sm().text_color(rgb(palette.text_muted)).child("Connection keys are edited in Storage. Save to validate and apply JSON changes."))
                    .child(div().flex_1().min_h_0().child(self.editor.clone()))
                    .child(div().flex().justify_end().child(button("save-json", "Save JSON", palette, cx, |view, _, cx| view.save_json(cx))))))
                .when(!self.json_mode, |el| el.child(self.render_form(palette, cx))))
            .when(self.modal.is_some(), |el| el.child(self.render_modal(palette, cx)))
    }
}
fn logging_edit(current: &Settings, index: usize, value: &str) -> anyhow::Result<Settings> {
    let mut settings = current.clone();
    let value = value.trim();
    let invalid = || anyhow::anyhow!("Enter a positive whole number.");
    match index {
        0 => settings.current_log_limit = value.parse().map_err(|_| invalid())?,
        1 => settings.logging.max_file_mib = value.parse().map_err(|_| invalid())?,
        2 => settings.logging.retention_days = value.parse().map_err(|_| invalid())?,
        3 => settings.logging.check_interval_hours = value.parse().map_err(|_| invalid())?,
        _ => anyhow::bail!("Unknown logging field"),
    }
    settings.validate()?;
    Ok(settings)
}

fn log_values(settings: &Settings) -> [String; 4] {
    [
        settings.current_log_limit.to_string(),
        settings.logging.max_file_mib.to_string(),
        settings.logging.retention_days.to_string(),
        settings.logging.check_interval_hours.to_string(),
    ]
}
fn setting_row(title: &'static str, description: &'static str, palette: Palette) -> Div {
    div()
        .flex()
        .items_center()
        .w_full()
        .gap_6()
        .py_5()
        .border_b_1()
        .border_color(rgb(palette.border))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(title)
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child(description),
                ),
        )
}
fn button(
    id: &'static str,
    label: &'static str,
    palette: Palette,
    cx: &Context<SettingsWindow>,
    action: impl Fn(&mut SettingsWindow, &mut Window, &mut Context<SettingsWindow>) + 'static,
) -> Stateful<Div> {
    let action = std::rc::Rc::new(action);
    let keyboard = action.clone();
    div()
        .id(id)
        .tab_index(0)
        .px_3()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .text_sm()
        .hover(|el| el.bg(rgb(palette.element_hover)))
        .focus(|el| el.border_color(rgb(palette.border_focused)))
        .on_click(cx.listener(move |view, _, window, cx| action(view, window, cx)))
        .on_key_down(
            cx.listener(move |view, event: &gpui::KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    keyboard(view, window, cx);
                    cx.stop_propagation();
                }
            }),
        )
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_edits_rebase_on_current_settings_without_changing_other_fields() {
        let settings = Settings {
            theme: ThemeMode::Dark,
            ..Default::default()
        };
        let settings = logging_edit(&settings, 1, "15").unwrap();
        let settings = logging_edit(&settings, 2, "14").unwrap();
        assert_eq!(settings.theme, ThemeMode::Dark);
        assert_eq!(settings.current_log_limit, 2000);
        assert_eq!(settings.logging.max_file_mib, 15);
        assert_eq!(settings.logging.retention_days, 14);
        assert_eq!(settings.logging.check_interval_hours, 24);
    }

    #[test]
    fn invalid_drafts_are_rejected_and_equivalent_text_is_already_saved() {
        let settings = Settings::default();
        for (index, value) in [
            (0, ""),
            (0, "-1"),
            (1, "0"),
            (1, "10241"),
            (2, "1.5"),
            (3, "no"),
        ] {
            assert!(logging_edit(&settings, index, value).is_err());
        }
        assert_eq!(logging_edit(&settings, 0, " 002000 ").unwrap(), settings);
        assert_eq!(logging_edit(&settings, 2, "007").unwrap(), settings);
    }
}
