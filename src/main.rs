use gpui::{
    App, Application, Bounds, Context, Div, FocusHandle, KeyBinding, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

mod backups;
mod backups_view;
mod database;
mod icons;
mod logging;
mod logs_view;
mod settings;
mod text_input;
mod theme;

use icons::{Assets, Icon};
use settings::Settings;
use theme::{Palette, ThemeMode};

actions!(
    workspace,
    [ToggleSidebar, ToggleLogs, FocusNext, FocusPrevious]
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Backups,
    Processes,
    Sandboxes,
    Settings,
}

impl Section {
    const ALL: [Self; 4] = [
        Self::Backups,
        Self::Processes,
        Self::Sandboxes,
        Self::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Backups => "Backups",
            Self::Processes => "Processes",
            Self::Sandboxes => "Sandboxes",
            Self::Settings => "Settings",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Backups => Icon::Archive,
            Self::Processes => Icon::Terminal,
            Self::Sandboxes => Icon::Box,
            Self::Settings => Icon::Settings,
        }
    }
}

struct Workspace {
    active_section: Section,
    sidebar_open: bool,
    focus_handle: FocusHandle,
    settings: Settings,
    settings_error: Option<String>,
    theme_focus: [FocusHandle; 3],
    backups: gpui::Entity<backups_view::BackupsView>,
    storage_fields: [gpui::Entity<text_input::TextInput>; 5],
    storage_message: Option<String>,
    logs: gpui::Entity<logs_view::LogsView>,
    logs_open: bool,
    logs_width: f32,
    resizing_logs: Option<(gpui::Pixels, f32)>,
    log_limit_input: gpui::Entity<text_input::TextInput>,
    log_limit_message: Option<String>,
}

impl Workspace {
    fn new(window: &mut Window, runtime: tokio::runtime::Handle, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        cx.observe_window_bounds(window, |this, _, cx| {
            this.logs.update(cx, |logs, cx| logs.remeasure(cx));
            cx.notify();
        })
        .detach();
        cx.observe_window_appearance(window, |this, _, cx| {
            if this.settings.theme == ThemeMode::System {
                this.backups.update(cx, |_, cx| cx.notify());
                this.logs.update(cx, |logs, cx| logs.remeasure(cx));
                this.log_limit_input.update(cx, |_, cx| cx.notify());
                for field in &this.storage_fields {
                    field.update(cx, |_, cx| cx.notify());
                }
                cx.notify();
            }
        })
        .detach();
        cx.observe_window_activation(window, |_, _, cx| cx.notify())
            .detach();
        let (settings, settings_error) = match Settings::load() {
            Ok(settings) => (settings, None),
            Err(error) => (
                Settings::default(),
                Some(format!(
                    "Could not load appearance settings: {error}. Select a theme to save again."
                )),
            ),
        };
        logging::store().set_limit(settings.current_log_limit);
        let logs = cx.new(|cx| logs_view::LogsView::new(settings.theme, cx));
        let log_limit_input = cx.new(|cx| {
            text_input::TextInput::new(
                settings.current_log_limit.to_string(),
                false,
                settings.theme,
                cx,
            )
        });
        let stored = database::load_storage();
        let storage_message = stored
            .as_ref()
            .err()
            .map(|error| format!("Could not load storage settings: {error}"));
        let config = stored.ok().flatten().unwrap_or_default();
        let values = [
            config.endpoint,
            config.bucket,
            config.region,
            config.access_key,
            config.secret_key,
        ];
        let storage_fields = std::array::from_fn(|index| {
            cx.new(|cx| {
                text_input::TextInput::new(values[index].clone(), index >= 3, settings.theme, cx)
            })
        });
        let backups = cx.new(|cx| backups_view::BackupsView::new(runtime, settings.theme, cx));
        Self {
            active_section: Section::Backups,
            sidebar_open: true,
            focus_handle,
            settings,
            settings_error,
            backups,
            storage_fields,
            storage_message,
            logs,
            logs_open: false,
            logs_width: 420.,
            resizing_logs: None,
            log_limit_input,
            log_limit_message: None,
            theme_focus: std::array::from_fn(|index| {
                cx.focus_handle()
                    .tab_index(10 + index as isize)
                    .tab_stop(true)
            }),
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        tracing::debug!(target:"app::ui",open=self.sidebar_open,"Navigation toggled");
        cx.notify();
    }

    fn set_theme(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        self.settings.theme = mode;
        self.logs.update(cx, |logs, cx| logs.set_theme(mode, cx));
        self.log_limit_input
            .update(cx, |field, cx| field.set_theme(mode, cx));
        tracing::info!(target:"app::settings",theme=mode.label(),"Appearance changed");
        for field in &self.storage_fields {
            field.update(cx, |field, cx| field.set_theme(mode, cx));
        }
        self.backups
            .update(cx, |backups, cx| backups.set_theme(mode, cx));
        self.settings_error = self.settings.save().err().map(|error| {
            tracing::error!(target:"app::settings",%error,"Could not save appearance");
            format!("Theme applied, but could not be saved: {error}. Select a theme to retry.")
        });
        cx.notify();
    }

    fn save_storage(&mut self, cx: &mut Context<Self>) {
        let values: Vec<_> = self
            .storage_fields
            .iter()
            .map(|field| field.read(cx).value())
            .collect();
        let config = backups::StorageConfig {
            endpoint: values[0].trim().into(),
            bucket: values[1].trim().into(),
            region: values[2].trim().into(),
            access_key: values[3].clone(),
            secret_key: values[4].clone(),
        };
        self.storage_message = Some(
            match self
                .backups
                .update(cx, |view, cx| view.configure(config, cx))
            {
                Ok(()) => "Connection saved on this computer. Checking backups…".into(),
                Err(error) => error,
            },
        );
        cx.notify();
    }

    fn toggle_logs(&mut self, cx: &mut Context<Self>) {
        self.logs_open = !self.logs_open;
        tracing::debug!(target:"app::ui",open=self.logs_open,"Log panel toggled");
        cx.notify();
    }
    fn save_log_limit(&mut self, cx: &mut Context<Self>) {
        let value = self.log_limit_input.read(cx).value();
        let Ok(limit) = value.trim().parse::<usize>() else {
            self.log_limit_message = Some("Enter a positive whole number.".into());
            cx.notify();
            return;
        };
        if limit == 0 {
            self.log_limit_message = Some("The limit must be greater than zero.".into());
            cx.notify();
            return;
        }
        let old = self.settings.current_log_limit;
        self.settings.current_log_limit = limit;
        match self.settings.save() {
            Ok(()) => {
                logging::store().set_limit(limit);
                tracing::info!(target:"app::settings",limit,"Current log limit changed");
                self.log_limit_message = Some("Saved. The new limit is active.".into());
            }
            Err(error) => {
                self.settings.current_log_limit = old;
                tracing::error!(target:"app::settings",%error,"Could not save log limit");
                self.log_limit_message = Some(format!("Could not save: {error}"));
            }
        }
        cx.notify();
    }

    fn render_settings(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-content")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .gap_4()
            .child(div().text_lg().child("Appearance"))
            .child(
                div().flex().flex_col().gap_2().child("Theme").child(
                    div()
                        .flex()
                        .flex_col()
                        .text_color(rgb(palette.text_muted))
                        .child("Choose a theme, or follow your system appearance.")
                        .child("Uses Zed’s Ayu Light and Ayu Mirage colors."),
                ),
            )
            .child(
                div()
                    .id("theme-tabs")
                    .flex()
                    .w_full()
                    .max_w(px(330.0))
                    .p_1()
                    .gap_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(palette.border))
                    .bg(rgb(palette.element_background))
                    .children(ThemeMode::ALL.into_iter().enumerate().map(|(index, mode)| {
                        let selected = self.settings.theme == mode;
                        div()
                            .id(mode.label())
                            .track_focus(&self.theme_focus[index])
                            .flex()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .h(px(32.0))
                            .rounded_sm()
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .cursor_pointer()
                            .text_color(rgb(if selected {
                                palette.text
                            } else {
                                palette.text_muted
                            }))
                            .when(selected, |tab| tab.bg(rgb(palette.element_selected)))
                            .when(!selected, |tab| {
                                tab.hover(|tab| tab.bg(rgb(palette.element_hover)))
                            })
                            .focus(|tab| tab.border_color(rgb(palette.border_focused)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.theme_focus[index].focus(window);
                                this.set_theme(mode, cx);
                            }))
                            .on_key_down(cx.listener(
                                move |this, event: &gpui::KeyDownEvent, window, cx| {
                                    let next_index = match event.keystroke.key.as_str() {
                                        "enter" | "space" => index,
                                        "left" => (index + 2) % 3,
                                        "right" => (index + 1) % 3,
                                        "home" => 0,
                                        "end" => 2,
                                        _ => return,
                                    };
                                    this.theme_focus[next_index].focus(window);
                                    this.set_theme(ThemeMode::ALL[next_index], cx);
                                    cx.stop_propagation();
                                },
                            ))
                            .child(mode.label())
                    })),
            )
            .child(div().pt_4().text_lg().child("Logging"))
            .child("Current log records")
            .child(div().text_sm().text_color(rgb(palette.text_muted)).child("Keep this many complete records in memory. Older records remain available in History."))
            .child(self.log_limit_input.clone())
            .child(div().id("save-log-limit").tab_index(19).px_3().py_2().rounded_sm().bg(rgb(palette.element_background)).cursor_pointer().hover(|el|el.bg(rgb(palette.element_hover))).child("Save log limit").on_click(cx.listener(|this,_,_,cx|this.save_log_limit(cx))).on_key_down(cx.listener(|this,event:&gpui::KeyDownEvent,_,cx|{if matches!(event.keystroke.key.as_str(),"enter"|"space"){this.save_log_limit(cx);cx.stop_propagation();}})))
            .children(self.log_limit_message.as_ref().map(|message|div().text_sm().child(message.clone())))
            .child(div().pt_4().text_lg().child("Storage"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child("Connection and keys are saved locally on this computer."),
            )
            .children(
                ["Endpoint", "Bucket", "Region", "Access key", "Secret key"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label)
                            .child(self.storage_fields[index].clone())
                    }),
            )
            .child(
                div()
                    .id("save-storage")
                    .tab_index(20)
                    .px_3()
                    .py_2()
                    .rounded_sm()
                    .bg(rgb(palette.element_background))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(palette.element_hover)))
                    .child("Save connection")
                    .on_click(cx.listener(|this, _, _, cx| this.save_storage(cx)))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.save_storage(cx);
                            cx.stop_propagation();
                        }
                    })),
            )
            .children(
                self.storage_message
                    .as_ref()
                    .map(|message| div().text_sm().child(message.clone())),
            )
            .children(
                self.settings_error
                    .as_ref()
                    .map(|error| div().text_color(rgb(palette.error)).child(error.clone())),
            )
    }

    fn render_sidebar(&self, palette: Palette, cx: &mut Context<Self>) -> Div {
        let active_section = self.active_section;

        div()
            .flex()
            .flex_col()
            .w(px(220.0))
            .h_full()
            .flex_shrink_0()
            .p_2()
            .gap_1()
            .bg(rgb(palette.panel_background))
            .border_r_1()
            .border_color(rgb(palette.border))
            .children(
                Section::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, section)| {
                        let selected = section == active_section;
                        div()
                            .id(section.label())
                            .tab_index(index as isize + 1)
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(34.0))
                            .px_3()
                            .rounded_sm()
                            .cursor_pointer()
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .focus(|item| item.border_color(rgb(palette.border_focused)))
                            .text_color(rgb(if selected {
                                palette.text
                            } else {
                                palette.text_muted
                            }))
                            .when(selected, |item| item.bg(rgb(palette.element_selected)))
                            .when(!selected, |item| {
                                item.hover(|item| item.bg(rgb(palette.element_hover)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.active_section = section;
                                tracing::debug!(target:"app::ui",section=section.label(),"Section opened");
                                cx.notify();
                            }))
                            .on_key_down(cx.listener(
                                move |this, event: &gpui::KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.active_section = section;
                                tracing::debug!(target:"app::ui",section=section.label(),"Section opened");
                                        cx.notify();
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .child(section.icon().render(if selected {
                                palette.icon
                            } else {
                                palette.icon_muted
                            }))
                            .child(section.label())
                    }),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.settings.theme.palette(window.appearance());
        let navigation_visible = self.sidebar_open
            && (!self.logs_open || f32::from(window.viewport_size().width) >= 1000.);
        let max_logs = (f32::from(window.viewport_size().width)
            - if navigation_visible { 220. } else { 0. }
            - 280.)
            .max(260.);
        let width = self.logs_width.clamp(260., max_logs);
        div()
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                    if let Some((start, width)) = this.resizing_logs {
                        if event.dragging() {
                            this.logs_width = (width + f32::from(start - event.position.x)).clamp(
                                260.,
                                (f32::from(window.viewport_size().width) - 280.).max(260.),
                            );
                            this.logs.update(cx, |logs, cx| logs.remeasure(cx));
                            cx.notify();
                        } else {
                            this.resizing_logs = None;
                        }
                    }
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing_logs.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.toggle_sidebar(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleLogs, _, cx| this.toggle_logs(cx)))
            .on_action(cx.listener(|_, _: &FocusNext, window, _| window.focus_next()))
            .on_action(cx.listener(|_, _: &FocusPrevious, window, _| window.focus_prev()))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(if window.is_window_active() {
                palette.title_bar_background
            } else {
                palette.title_bar_inactive_background
            }))
            .text_color(rgb(palette.text))
            .text_base()
            .when(
                cfg!(target_os = "macos") && !window.is_fullscreen(),
                |root| root.pt(px(40.0)),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .bg(rgb(palette.editor_background))
                    .when(navigation_visible, |row| {
                        row.child(self.render_sidebar(palette, cx))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .h(px(44.0))
                                    .flex_shrink_0()
                                    .px_4()
                                    .border_b_1()
                                    .border_color(rgb(palette.border))
                                    .child(self.active_section.label()),
                            )
                            .when(self.active_section == Section::Settings, |content| {
                                content.child(self.render_settings(palette, cx))
                            })
                            .when(self.active_section == Section::Backups, |content| {
                                content.child(self.backups.clone())
                            }),
                    )
                    .when(self.logs_open, |row| {
                        row.child(
                            div()
                                .relative()
                                .w(px(width))
                                .h_full()
                                .min_h_0()
                                .flex_shrink_0()
                                .border_l_1()
                                .border_color(rgb(palette.border))
                                .child(self.logs.clone())
                                .child(
                                    div()
                                        .id("resize-logs")
                                        .absolute()
                                        .left(px(-3.))
                                        .top_0()
                                        .bottom_0()
                                        .w(px(6.))
                                        .cursor(gpui::CursorStyle::ResizeLeftRight)
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(
                                                move |this, event: &gpui::MouseDownEvent, _, cx| {
                                                    this.resizing_logs =
                                                        Some((event.position.x, width));
                                                    cx.stop_propagation();
                                                },
                                            ),
                                        ),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(32.0))
                    .flex_shrink_0()
                    .px_2()
                    .bg(rgb(palette.status_bar_background))
                    .border_t_1()
                    .border_color(rgb(palette.border))
                    .child(
                        div()
                            .id("toggle-sidebar")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(24.0))
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(rgb(palette.text_muted))
                            .hover(|button| {
                                button
                                    .bg(rgb(palette.element_hover))
                                    .text_color(rgb(palette.text))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_sidebar(cx);
                            }))
                            .child(
                                if self.sidebar_open {
                                    Icon::SidebarOpen
                                } else {
                                    Icon::SidebarClosed
                                }
                                .render(palette.icon)
                                .size(px(14.0)),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("toggle-logs")
                            .tab_index(30)
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(24.))
                            .rounded_sm()
                            .cursor_pointer()
                            .when(self.logs_open, |el| el.bg(rgb(palette.element_selected)))
                            .hover(|el| el.bg(rgb(palette.element_hover)))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_logs(cx)))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.toggle_logs(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .child(Icon::Debug.render(palette.icon).size(px(14.))),
                    ),
            )
    }
}

fn main() {
    logging::init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Не удалось запустить фоновые задачи");
    let runtime_handle = runtime.handle().clone();
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            text_input::bind_keys(cx);
            cx.bind_keys([
                KeyBinding::new("cmd-shift-l", ToggleLogs, Some("Workspace")),
                KeyBinding::new("ctrl-shift-l", ToggleLogs, Some("Workspace")),
                KeyBinding::new("cmd-b", ToggleSidebar, Some("Workspace")),
                KeyBinding::new("ctrl-b", ToggleSidebar, Some("Workspace")),
                KeyBinding::new("tab", FocusNext, Some("Workspace")),
                KeyBinding::new("shift-tab", FocusPrevious, Some("Workspace")),
            ]);

            let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(640.0), px(400.0))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Sandbox Prepare".into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Workspace::new(window, runtime_handle, cx)),
            )
            .expect("Не удалось открыть окно");

            tracing::info!(target:"app::startup","Application window opened");
            cx.activate(true);
        });
    tracing::info!(target:"app::startup","Application stopping");
    let _ = logging::store().flush();
}
