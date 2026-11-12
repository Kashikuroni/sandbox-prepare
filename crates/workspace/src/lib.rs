pub use sandbox_backups::{backup_cache, backups};
pub use sandbox_platform::database;
pub use sandbox_sandboxes::sandbox;
pub use sandbox_settings::settings;
pub use sandbox_telemetry::logging;
pub use sandbox_telemetry::process_log;
pub use sandbox_ui::{icons, text_input, theme};
mod logs_view;
mod sandboxes_view;
mod settings_view;
use crate::theme::ThemePalette as _;
use gpui::{
    App, Application, Bounds, Context, Div, FocusHandle, KeyBinding, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

mod backups_view;
use icons::{Assets, Icon};
use settings::{Settings, SettingsModel};
use theme::{Palette, ThemeMode};

actions!(
    workspace,
    [
        ToggleSidebar,
        ToggleLogs,
        OpenSettings,
        FocusNext,
        FocusPrevious,
        QuitSandbox
    ]
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Backups,
    Processes,
    Sandboxes,
}

impl Section {
    const ALL: [Self; 3] = [Self::Backups, Self::Processes, Self::Sandboxes];

    fn label(self) -> &'static str {
        match self {
            Self::Backups => "Backups",
            Self::Processes => "Processes",
            Self::Sandboxes => "Sandboxes",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Backups => Icon::Archive,
            Self::Processes => Icon::Terminal,
            Self::Sandboxes => Icon::Box,
        }
    }
}

struct Workspace {
    active_section: Section,
    sidebar_open: bool,
    focus_handle: FocusHandle,
    settings: Settings,
    settings_model: gpui::Entity<SettingsModel>,
    settings_window: Option<gpui::WindowHandle<settings_view::SettingsWindow>>,
    backups: gpui::Entity<backups_view::BackupsView>,
    sandbox: gpui::Entity<sandboxes_view::SandboxesView>,
    logs: gpui::Entity<logs_view::LogsView>,
    logs_open: bool,
    logs_width: f32,
    resizing_logs: Option<(gpui::Pixels, f32)>,
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
                cx.notify();
            }
        })
        .detach();
        cx.observe_window_activation(window, |_, _, cx| cx.notify())
            .detach();
        let settings_model = cx.new(|_| SettingsModel::load());
        let settings = settings_model.read(cx).value.clone();
        cx.observe(&settings_model, |this, model, cx| {
            this.settings = model.read(cx).value.clone();
            let theme = this.settings.theme;
            this.logs.update(cx, |logs, cx| logs.set_theme(theme, cx));
            this.sandbox.update(cx, |sandbox, cx| {
                sandbox.configure_logs(&this.settings);
                sandbox.set_theme(theme, cx);
            });
            this.backups.update(cx, |backups, cx| {
                backups.set_theme(theme, cx);
            });
            cx.notify();
        })
        .detach();
        let logs = cx.new(|cx| logs_view::LogsView::new(settings.theme, cx));
        let backups = cx.new(|cx| backups_view::BackupsView::new(runtime, settings.theme, cx));
        let sandbox = cx.new(|cx| sandboxes_view::SandboxesView::new(&settings, cx));
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            weak.update(cx, |workspace, cx| {
                if workspace.sandbox.read(cx).may_close() {
                    true
                } else {
                    workspace
                        .sandbox
                        .update(cx, |sandbox, cx| sandbox.request_exit(cx));
                    false
                }
            })
            .unwrap_or(true)
        });
        cx.on_app_quit(|workspace, cx| {
            workspace.sandbox.read(cx).shutdown_fallback();
            async {}
        })
        .detach();

        Self {
            active_section: Section::Backups,
            sidebar_open: true,
            focus_handle,
            settings,
            settings_model,
            settings_window: None,
            backups,
            sandbox,
            logs,
            logs_open: false,
            logs_width: 420.,
            resizing_logs: None,
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        tracing::debug!(target:"app::ui",open=self.sidebar_open,"Navigation toggled");
        cx.notify();
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.settings_window
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let model = self.settings_model.clone();
        let backups = self.backups.clone();
        let bounds = Bounds::centered(None, size(px(1000.), px(740.)), cx);
        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(760.), px(500.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Settings — Sandbox Prepare".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| settings_view::SettingsWindow::new(model, backups, window, cx))
            },
        ) {
            Ok(handle) => {
                self.settings_window = Some(handle);
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            Err(error) => {
                tracing::error!(target: "app::ui", %error, "Could not open settings window");
            }
        }
    }

    fn toggle_logs(&mut self, cx: &mut Context<Self>) {
        self.logs_open = !self.logs_open;
        tracing::debug!(target:"app::ui",open=self.logs_open,"Log panel toggled");
        cx.notify();
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
                                this.sandbox.update(cx, |sandbox, cx| sandbox.show_processes(section == Section::Processes, cx));
                                tracing::debug!(target:"app::ui",section=section.label(),"Section opened");
                                cx.notify();
                            }))
                            .on_key_down(cx.listener(
                                move |this, event: &gpui::KeyDownEvent, _, cx| {
                                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                        this.active_section = section;
                                this.sandbox.update(cx, |sandbox, cx| sandbox.show_processes(section == Section::Processes, cx));
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
            .on_action(cx.listener(|this, _: &QuitSandbox, _, cx| {
                this.sandbox
                    .update(cx, |sandbox, cx| sandbox.request_exit(cx))
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.open_settings(cx)))
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(40.))
                    .flex_shrink_0()
                    .px_2()
                    .child(
                        div()
                            .id("titlebar-drag")
                            .h_full()
                            .flex_1()
                            .window_control_area(gpui::WindowControlArea::Drag)
                            .on_mouse_down(gpui::MouseButton::Left, |event, window, _| {
                                if event.click_count == 2 {
                                    window.titlebar_double_click();
                                } else {
                                    window.start_window_move();
                                }
                            }),
                    )
                    .child(
                        div()
                            .id("open-settings")
                            .tab_index(31)
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.))
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|el| el.bg(rgb(palette.element_hover)))
                            .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.open_settings(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .child(Icon::Settings.render(palette.icon)),
                    ),
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
                            .when(self.active_section == Section::Backups, |content| {
                                content.child(self.backups.clone())
                            })
                            .when(self.active_section != Section::Backups, |content| {
                                content.child(self.sandbox.clone())
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

pub fn run() {
    let preferences = Settings::load();
    let logging_directory = database::database_path()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("logs")));
    let initial = preferences.as_ref().cloned().unwrap_or_default();
    logging::init(
        logging_directory,
        initial.current_log_limit,
        initial.logging,
    );
    if let Err(error) = preferences {
        tracing::error!(target:"app::settings", %error, "Could not load settings; defaults applied");
    }
    if let Ok(Some(config)) = database::load_storage() {
        logging::register_credentials(&config.access_key, &config.secret_key);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Не удалось запустить фоновые задачи");
    let runtime_handle = runtime.handle().clone();
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            text_input::bind_keys(cx);
            cx.set_menus(vec![gpui::Menu {
                name: "Sandbox Prepare".into(),
                items: vec![
                    gpui::MenuItem::action("Settings…", OpenSettings),
                    gpui::MenuItem::separator(),
                    gpui::MenuItem::action("Quit Sandbox Prepare", QuitSandbox),
                ],
            }]);
            cx.bind_keys([
                KeyBinding::new("cmd-q", QuitSandbox, None),
                KeyBinding::new("ctrl-q", QuitSandbox, None),
                KeyBinding::new("cmd-,", OpenSettings, Some("Workspace")),
                KeyBinding::new("ctrl-,", OpenSettings, Some("Workspace")),
                KeyBinding::new("cmd-shift-l", ToggleLogs, Some("Workspace")),
                KeyBinding::new("ctrl-shift-l", ToggleLogs, Some("Workspace")),
                KeyBinding::new("cmd-b", ToggleSidebar, Some("Workspace")),
                KeyBinding::new("ctrl-b", ToggleSidebar, Some("Workspace")),
                KeyBinding::new("tab", FocusNext, Some("Workspace")),
                KeyBinding::new("shift-tab", FocusPrevious, Some("Workspace")),
                KeyBinding::new("tab", FocusNext, Some("Settings")),
                KeyBinding::new("shift-tab", FocusPrevious, Some("Settings")),
            ]);

            let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);

            let workspace_window = cx
                .open_window(
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
            cx.on_action(move |_: &OpenSettings, cx| {
                let _ = workspace_window.update(cx, |workspace, _, cx| workspace.open_settings(cx));
            });
            cx.on_action(move |_: &QuitSandbox, cx| {
                let _ = workspace_window.update(cx, |workspace, _, cx| {
                    workspace
                        .sandbox
                        .update(cx, |sandbox, cx| sandbox.request_exit(cx));
                });
            });

            tracing::info!(target:"app::startup","Application window opened");
            cx.activate(true);
        });
    tracing::info!(target:"app::startup","Application stopping");
    let _ = logging::store().flush();
}
