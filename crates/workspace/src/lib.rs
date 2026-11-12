pub use sandbox_backups::{backup_cache, backups};
pub use sandbox_platform::database;
use sandbox_runtime::Runtime;
pub use sandbox_sandboxes::sandbox;
pub use sandbox_settings::settings;
pub use sandbox_telemetry::logging;
pub use sandbox_telemetry::process_log;
pub use sandbox_ui::{icons, text_input, theme};
use std::sync::Arc;
mod context_bar;
mod logs_view;
mod projects_view;
mod source_menu;
use sandbox_core::models::{ProjectId, SandboxId};
use sandbox_ui::{context_picker::ContextPicker, titlebar_style as titlebar};
use std::collections::BTreeMap;
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
    Projects,
}

impl Section {
    const ALL: [Self; 4] = [
        Self::Backups,
        Self::Processes,
        Self::Sandboxes,
        Self::Projects,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Backups => "Backups",
            Self::Processes => "Processes",
            Self::Sandboxes => "Sandboxes",
            Self::Projects => "Projects",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Backups => Icon::Archive,
            Self::Processes => Icon::Terminal,
            Self::Sandboxes => Icon::Box,
            Self::Projects => Icon::Settings,
        }
    }
}

struct Workspace {
    application: Arc<Runtime>,
    project_id: ProjectId,
    sandbox_id: Option<SandboxId>,
    project_picker: gpui::Entity<ContextPicker>,
    source_menu: gpui::Entity<source_menu::SourceMenu>,
    context_bar: context_bar::ContextBar,
    projects: gpui::Entity<projects_view::ProjectsView>,
    backup_views: BTreeMap<ProjectId, gpui::Entity<backups_view::BackupsView>>,
    context_error: Option<String>,
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
    fn new(window: &mut Window, application: Arc<Runtime>, cx: &mut Context<Self>) -> Self {
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
                this.projects.update(cx, |_, cx| cx.notify());
                this.project_picker.update(cx, |_, cx| cx.notify());
                this.source_menu.update(cx, |_, cx| cx.notify());
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
            this.projects
                .update(cx, |view, cx| view.set_theme(this.settings.theme, cx));
            this.project_picker
                .update(cx, |view, cx| view.set_theme(this.settings.theme, cx));
            this.source_menu
                .update(cx, |view, cx| view.set_theme(this.settings.theme, cx));
            this.application.configure(&this.settings);
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
        let mut projects = application.catalog.projects().expect("Opened catalog");
        let recent = application
            .catalog
            .recent_project_ids()
            .expect("Opened project history");
        let index = recent
            .iter()
            .find_map(|id| projects.iter().position(|p| &p.id == id))
            .unwrap_or(0);
        let project = projects.remove(index);
        let service = application
            .backups(&project.id)
            .expect("Opened backup service");
        let rows = application
            .catalog
            .sandboxes()
            .expect("Opened sandbox catalog");
        let record = rows
            .into_iter()
            .find(|sandbox| sandbox.project_id == project.id);
        let manager = record
            .as_ref()
            .and_then(|record| application.manager(&record.id).ok());
        let backups = cx.new(|cx| backups_view::BackupsView::new(service, settings.theme, cx));
        let sandbox = cx.new(|cx| {
            sandboxes_view::SandboxesView::new(application.clone(), manager, &settings, cx)
        });
        cx.subscribe(
            &sandbox,
            |this, _, _: &sandboxes_view::ConfigureLaunch, cx| {
                this.active_section = Section::Projects;
                this.projects.update(cx, |view, cx| view.show_launch(cx));
                cx.notify();
            },
        )
        .detach();
        cx.subscribe(
            &sandbox,
            |this, _, _: &sandboxes_view::ViewProcesses, cx| {
                this.active_section = Section::Processes;
                cx.notify();
            },
        )
        .detach();
        let project_id = project.id.clone();
        let sandbox_id = record.as_ref().map(|record| record.id.clone());
        let projects = cx.new(|cx| {
            projects_view::ProjectsView::new(
                application.clone(),
                project_id.clone(),
                sandbox_id.clone(),
                settings.theme,
                cx,
            )
        });
        let project_picker =
            Self::new_context_picker("Project", "Search projects…", None, settings.theme, cx);
        let source_menu = cx.new(|cx| source_menu::SourceMenu::new(settings.theme, cx));
        cx.subscribe(&projects, |this, _, event: &projects_view::Changed, cx| {
            this.select_context(event.project.clone(), event.sandbox.clone(), cx);
        })
        .detach();
        let backup_views = BTreeMap::from([(project_id.clone(), backups.clone())]);
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            weak.update(cx, |workspace, _cx| {
                if workspace.application.exit_ready() {
                    true
                } else {
                    workspace.application.begin_shutdown();
                    false
                }
            })
            .unwrap_or(true)
        });
        cx.on_app_quit(|workspace, _cx| {
            if let Err(error)=workspace.application.shutdown_blocking() {tracing::error!(target:"app::sandbox",%error,"Shutdown failed; watchdog will retry");}
            async {}
        })
        .detach();

        cx.spawn(async move |workspace, cx| {
            let mut choices = std::time::Instant::now();
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(200)).await;
                if workspace
                    .update(cx, |workspace, cx| {
                        workspace.poll_context_events(cx);
                        if choices.elapsed() > std::time::Duration::from_secs(2)
                            && !workspace.application.is_stopping()
                        {
                            choices = std::time::Instant::now();
                            workspace.refresh_choices(cx);
                            #[cfg(debug_assertions)]
                            if std::env::var_os("SANDBOX_PREPARE_TEST_DATA_DIR").is_some()
                                && std::env::var_os("SANDBOX_PREPARE_UI_PREVIEW").is_some()
                            {
                                match std::env::var("SANDBOX_PREPARE_UI_PREVIEW_SCREEN").as_deref()
                                {
                                    Ok("launch") => {
                                        workspace.active_section = Section::Projects;
                                        workspace
                                            .projects
                                            .update(cx, |view, cx| view.show_launch(cx));
                                    }
                                    Ok("sandbox") => workspace.active_section = Section::Sandboxes,
                                    _ => {}
                                }
                            }
                        }
                        if workspace.application.exit_ready() {
                            cx.quit();
                            return;
                        }
                        if workspace.application.is_stopping() {
                            workspace
                                .sandbox
                                .update(cx, |view, cx| view.set_stopping(true, cx));
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut workspace = Self {
            application,
            project_id,
            sandbox_id,
            project_picker,
            source_menu,
            context_bar: Default::default(),
            projects,
            backup_views,
            context_error: None,
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
        };
        workspace.configure_context_actions(cx);
        workspace.bind_context_pickers(cx);
        workspace.refresh_choices(cx);
        #[cfg(debug_assertions)]
        if std::env::var_os("SANDBOX_PREPARE_TEST_DATA_DIR").is_some()
            && std::env::var_os("SANDBOX_PREPARE_UI_PREVIEW").is_some()
        {
            match std::env::var("SANDBOX_PREPARE_UI_PREVIEW_SCREEN").as_deref() {
                Ok("launch") => {
                    workspace.active_section = Section::Projects;
                    workspace
                        .projects
                        .update(cx, |view, cx| view.show_launch(cx));
                }
                Ok("sandbox") => workspace.active_section = Section::Sandboxes,
                _ => {}
            }
        }

        #[cfg(debug_assertions)]
        if std::env::var_os("SANDBOX_PREPARE_TEST_DATA_DIR").is_some()
            && std::env::var_os("SANDBOX_PREPARE_UI_PREVIEW").is_some()
            && let Ok(menu) = std::env::var("SANDBOX_PREPARE_UI_PREVIEW_MENU")
        {
            cx.defer_in(window, move |workspace, window, cx| {
                if menu == "project" {
                    workspace
                        .project_picker
                        .update(cx, |p, cx| p.open(window, cx));
                } else {
                    workspace.source_menu.update(cx, |p, cx| p.open(window, cx));
                }
            });
        }
        workspace
    }

    fn select_context(
        &mut self,
        project: ProjectId,
        sandbox: Option<SandboxId>,
        cx: &mut Context<Self>,
    ) {
        let result = (|| {
            let service = self.application.backups(&project)?;
            if let Some(handle) = self.settings_window {
                handle
                    .update(cx, |view, _, cx| view.switch_project(service.clone(), cx))
                    .unwrap_or(Ok(()))?;
            }
            let manager = sandbox
                .as_ref()
                .map(|id| self.application.manager(id))
                .transpose()?;
            let backups = if let Some(view) = self.backup_views.get(&project) {
                view.clone()
            } else {
                let view =
                    cx.new(|cx| backups_view::BackupsView::new(service, self.settings.theme, cx));
                self.backup_views.insert(project.clone(), view.clone());
                view
            };
            backups.update(cx, |view, cx| view.set_theme(self.settings.theme, cx));
            self.backups = backups;
            self.sandbox
                .update(cx, |view, cx| view.set_manager(manager, cx));
            self.context_bar
                .last_sandbox
                .insert(self.project_id.clone(), self.sandbox_id.clone());
            self.context_bar
                .last_sandbox
                .insert(project.clone(), sandbox.clone());
            self.project_id = project.clone();
            self.sandbox_id = sandbox.clone();
            self.projects
                .update(cx, |view, cx| view.context(project.clone(), sandbox, cx));
            self.application.catalog.record_project_open(&project)?;
            Ok::<_, anyhow::Error>(())
        })();
        self.context_error = result.err().map(|e| e.to_string());
        self.refresh_choices(cx);
        cx.notify();
    }
    fn run_current_sandbox(&mut self, cx: &mut Context<Self>) {
        self.active_section = Section::Sandboxes;
        if let Some(id) = &self.sandbox_id {
            self.context_error = self
                .application
                .run_sandbox(id, false)
                .err()
                .map(|e| logging::redact(&format!("{e:#}")));
        } else {
            self.context_bar
                .run_after_source
                .insert(self.project_id.clone());
            self.open_current_for_run(cx);
        }
        cx.notify();
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
        let backups = self.backups.read(cx).service();
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
        let launch_busy = self
            .sandbox_id
            .as_ref()
            .and_then(|id| self.application.sandbox_status(id))
            .is_some_and(|s| s.busy || s.shutting_down)
            || self.application.is_stopping();
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
            .on_action(cx.listener(|this, _: &QuitSandbox, _, _| {
                this.application.begin_shutdown();
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
                    .h(px(titlebar::TITLEBAR_HEIGHT))
                    .flex_shrink_0()
                    .pl(px(
                        if window.is_fullscreen() || !cfg!(target_os = "macos") {
                            8.
                        } else {
                            titlebar::TRAFFIC_LIGHT_PADDING
                        },
                    ))
                    .pr(px(8.))
                    .child(self.render_context_bar(window))
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
                            .id("run-current-sandbox")
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .focus(|d| d.border_color(rgb(palette.border_focused)))
                            .w(px(110.))
                            .justify_center()
                            .when(launch_busy, |d| d.opacity(0.5))
                            .tab_index(30)
                            .h(px(titlebar::BUTTON_HEIGHT))
                            .px(px(6.))
                            .flex()
                            .items_center()
                            .rounded_sm()
                            .text_size(px(12.))
                            .font_family(titlebar::FONT_FAMILY)
                            .cursor_pointer()
                            .hover(|d| d.bg(rgb(palette.element_hover)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !launch_busy {
                                    this.run_current_sandbox(cx)
                                }
                            }))
                            .on_key_down(cx.listener(
                                move |this, event: &gpui::KeyDownEvent, _, cx| {
                                    if !launch_busy
                                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                    {
                                        this.run_current_sandbox(cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .child("Start sandbox"),
                    )
                    .child(
                        div()
                            .id("open-settings")
                            .tab_index(31)
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(titlebar::BUTTON_HEIGHT))
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
                            .child(Icon::Settings.render(palette.icon).size(px(14.))),
                    ),
            )
            .children(self.application.shutdown_status().error.map(|error| {
                div()
                    .px_4()
                    .py_2()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(error)
            }))
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
                            .when(
                                matches!(
                                    self.active_section,
                                    Section::Sandboxes | Section::Processes
                                ),
                                |content| content.child(self.sandbox.clone()),
                            )
                            .when(self.active_section == Section::Projects, |content| {
                                content.child(self.projects.clone())
                            })
                            .when_some(self.context_error.clone(), |content, error| {
                                content
                                    .child(div().p_3().text_color(rgb(palette.error)).child(error))
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
    let application = match Runtime::open(runtime.handle().clone(), &initial) {
        Ok(application) => application,
        Err(error) => {
            tracing::error!(target:"app::startup",%error,"Could not open application runtime");
            eprintln!("Could not start Sandbox Prepare: {error:#}");
            return;
        }
    };
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            titlebar::register_font(cx).expect("Register bundled Zed UI font");
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

            let mut initial_size = size(px(1100.0), px(720.0));
            if cfg!(debug_assertions)
                && std::env::var_os("SANDBOX_PREPARE_TEST_DATA_DIR").is_some()
                && std::env::var_os("SANDBOX_PREPARE_UI_PREVIEW").is_some()
                && std::env::var("SANDBOX_PREPARE_UI_PREVIEW_SIZE").as_deref() == Ok("minimum")
            {
                initial_size = size(px(640.), px(400.));
            }
            let bounds = Bounds::centered(None, initial_size, cx);

            let workspace_window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(640.0), px(400.0))),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Sandbox Prepare".into()),
                            appears_transparent: true,
                            traffic_light_position: Some(gpui::point(px(9.), px(9.))),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| cx.new(|cx| Workspace::new(window, application.clone(), cx)),
                )
                .expect("Не удалось открыть окно");
            cx.on_action(move |_: &OpenSettings, cx| {
                let _ = workspace_window.update(cx, |workspace, _, cx| workspace.open_settings(cx));
            });
            cx.on_action(move |_: &QuitSandbox, cx| {
                let _ = workspace_window.update(cx, |workspace, _, _cx| {
                    workspace.application.begin_shutdown();
                });
            });

            tracing::info!(target:"app::startup","Application window opened");
            cx.activate(true);
        });
    tracing::info!(target:"app::startup","Application stopping");
    let _ = logging::store().flush();
}
