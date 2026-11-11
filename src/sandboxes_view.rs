use crate::{
    icons::Icon,
    process_log::Line,
    sandbox::{Action, Manager},
    theme::{Palette, ThemeMode},
};
use gpui::{
    ClipboardItem, Context, Div, FocusHandle, ListAlignment, ListState, Render, Window, div, list,
    prelude::*, px, rgb,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
#[derive(Clone, Copy)]
enum Confirmation {
    Recreate,
    Delete,
}
pub struct SandboxesView {
    manager: Option<Arc<Manager>>,
    error: Option<String>,
    theme: ThemeMode,
    processes: bool,
    revision: u64,
    log_revision: u64,
    lines: Vec<Line>,
    list: ListState,
    filter: usize,
    confirmation: Option<Confirmation>,
    focus: FocusHandle,
    modal_focus: [FocusHandle; 2],
    modal_index: usize,
    exit_requested: bool,
    paused: bool,
    copied: bool,
}
impl SandboxesView {
    pub fn new(settings: &crate::settings::Settings, cx: &mut Context<Self>) -> Self {
        let theme = settings.theme;
        let manager = Manager::open();
        if let Ok(manager) = &manager {
            manager
                .logs
                .configure(settings.current_log_limit, settings.logging);
        }
        let error = manager.as_ref().err().map(|e| format!("{e:#}"));
        cx.spawn(async move |view, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(200)).await;
                if view
                    .update(cx, |view, cx| {
                        let Some(manager) = view.manager.clone() else {
                            return;
                        };
                        let revision = manager.revision.load(Ordering::Acquire);
                        let logs = manager.logs.revision.load(Ordering::Acquire);
                        if view.exit_requested && manager.exit_ready() {
                            cx.quit();
                            return;
                        }
                        if revision != view.revision || (!view.paused && logs != view.log_revision)
                        {
                            view.revision = revision;
                            if !view.paused {
                                view.refresh_lines();
                            }
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
        Self {
            manager: manager.ok(),
            error,
            theme,
            processes: false,
            revision: 0,
            log_revision: 0,
            lines: vec![],
            list: ListState::new(0, ListAlignment::Bottom, px(400.)),
            filter: 0,
            confirmation: None,
            focus: cx.focus_handle(),
            modal_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            modal_index: 0,
            exit_requested: false,
            paused: false,
            copied: false,
        }
    }
    pub fn configure_logs(&self, settings: &crate::settings::Settings) {
        if let Some(manager) = &self.manager {
            manager
                .logs
                .configure(settings.current_log_limit, settings.logging);
        }
    }
    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }
    pub fn show_processes(&mut self, show: bool, cx: &mut Context<Self>) {
        self.processes = show;
        cx.notify();
    }
    pub fn request_exit(&mut self, cx: &mut Context<Self>) {
        self.exit_requested = true;
        if let Some(manager) = &self.manager {
            manager.begin_shutdown();
        } else {
            cx.quit();
        }
        cx.notify();
    }
    pub fn may_close(&self) -> bool {
        self.exit_requested && self.manager.as_ref().is_none_or(|m| m.exit_ready())
    }
    pub fn shutdown_fallback(&self) {
        if let Some(manager) = &self.manager
            && let Err(error) = manager.shutdown_blocking()
        {
            tracing::error!(target:"app::sandbox",%error,"Shutdown cleanup failed; watchdog will retry");
        }
    }
    fn refresh_lines(&mut self) {
        let Some(manager) = &self.manager else {
            return;
        };
        self.log_revision = manager.logs.revision.load(Ordering::Acquire);
        let lines: Vec<_> = manager
            .logs
            .snapshot()
            .into_iter()
            .filter(|line| match self.filter {
                1 => line.process == "mysql",
                2 => line.process == "clickhouse",
                3 => line.process != "mysql" && line.process != "clickhouse",
                _ => true,
            })
            .collect();
        if let Some(first) = lines.first() {
            let removed = self
                .lines
                .iter()
                .take_while(|line| line.seq < first.seq)
                .count();
            let retained = self.lines.len() - removed;
            if retained <= lines.len() && self.lines.get(removed).map(|l| l.seq) == Some(first.seq)
            {
                self.list.splice(0..removed, 0);
                self.list.splice(retained..retained, lines.len() - retained);
            } else {
                self.list.reset(lines.len());
            }
        } else {
            self.list.reset(0);
        }
        self.lines = lines;
    }
    fn act(&mut self, action: Action, cx: &mut Context<Self>) {
        if let Some(manager) = &self.manager {
            manager.launch(action);
        }
        self.copied = false;
        cx.notify();
    }
    fn confirm(&mut self, confirmation: Confirmation, window: &mut Window, cx: &mut Context<Self>) {
        self.confirmation = Some(confirmation);
        self.modal_index = 0;
        self.modal_focus[0].focus(window);
        cx.notify();
    }
    fn log_row(&self, index: usize, palette: Palette) -> Div {
        let Some(line) = self.lines.get(index) else {
            return div();
        };
        div()
            .w_full()
            .min_w_0()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(palette.text_muted))
                    .child(format!("{} · {}", line.time, line.process)),
            )
            .child(div().text_sm().whitespace_normal().child(line.text.clone()))
    }
    fn sandbox_page(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let state = self
            .manager
            .as_ref()
            .map(|m| m.snapshot())
            .unwrap_or_default();
        let busy = state.busy || state.shutting_down || self.exit_requested;
        let active = state.active.as_ref();
        let mut page = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .p_4()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button("sandbox-refresh", "", palette, !busy, cx, |view, _, cx| {
                            view.act(Action::Refresh, cx)
                        })
                        .w(px(28.))
                        .p_0()
                        .child(Icon::Refresh.render(palette.icon)),
                    )
                    .child(div().flex_1())
                    .child(button(
                        "sandbox-create",
                        if active.is_some() {
                            "Recreate"
                        } else {
                            "Create from backups"
                        },
                        palette,
                        !busy && state.docker_ready,
                        cx,
                        move |view, window, cx| {
                            if view
                                .manager
                                .as_ref()
                                .is_some_and(|m| m.snapshot().active.is_some())
                            {
                                view.confirm(Confirmation::Recreate, window, cx);
                            } else {
                                view.act(Action::Create, cx);
                            }
                        },
                    ))
                    .child(button(
                        "sandbox-start",
                        "Start",
                        palette,
                        !busy && active.is_some() && state.running != [true; 2],
                        cx,
                        |view, _, cx| view.act(Action::Start, cx),
                    ))
                    .child(button(
                        "sandbox-stop",
                        "Stop",
                        palette,
                        !busy && state.running.contains(&true),
                        cx,
                        |view, _, cx| view.act(Action::Stop, cx),
                    ))
                    .child(button(
                        "sandbox-delete",
                        "Delete…",
                        palette,
                        !busy && active.is_some(),
                        cx,
                        |view, window, cx| view.confirm(Confirmation::Delete, window, cx),
                    )),
            )
            .child(div().text_sm().text_color(rgb(palette.text_muted)).child(
                if state.stage.is_empty() {
                    "Checking Docker…".into()
                } else {
                    state.stage.clone()
                },
            ))
            .when(state.busy && !self.exit_requested, |page| {
                page.child(button(
                    "sandbox-cancel",
                    "Cancel operation",
                    palette,
                    true,
                    cx,
                    |view, _, cx| {
                        if let Some(manager) = &view.manager {
                            manager.cancel();
                        }
                        cx.notify();
                    },
                ))
            })
            .children(state.error.iter().chain(self.error.iter()).map(|error| {
                div()
                    .text_sm()
                    .text_color(rgb(palette.error))
                    .child(error.clone())
            }));
        if state.stage.starts_with("Extracting")
            && let Some(manager) = &self.manager
        {
            page = page.child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child(format!(
                        "Expanded: {}",
                        crate::backups::format_size(
                            manager.extracted_bytes.load(Ordering::Acquire)
                        )
                    )),
            );
        }
        if let Some(instance) = active {
            for (index, title, version, endpoint) in [
                (
                    0,
                    "MySQL",
                    "Percona 8.4.11-11",
                    format!("127.0.0.1:{}", instance.ports[0]),
                ),
                (
                    1,
                    "ClickHouse",
                    "26.8.19.9",
                    format!(
                        "HTTP 127.0.0.1:{} · Native 127.0.0.1:{}",
                        instance.ports[1], instance.ports[2]
                    ),
                ),
            ] {
                page = page.child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .gap_1()
                        .py_3()
                        .border_b_1()
                        .border_color(rgb(palette.border))
                        .child(div().flex().items_center().gap_3().child(title).child(
                            div().text_sm().text_color(rgb(palette.text_muted)).child(
                                if state.running[index] {
                                    "Running"
                                } else {
                                    "Stopped"
                                },
                            ),
                        ))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(palette.text_muted))
                                .child(version),
                        )
                        .child(div().text_sm().child(endpoint))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(palette.text_muted))
                                .child(format!(
                                    "{} restored tables · user: sandbox",
                                    instance.tables[index]
                                )),
                        )
                        .child(
                            div()
                                .text_sm()
                                .w_full()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(instance.backups[if index == 0 { 1 } else { 0 }].clone()),
                        ),
                );
            }
            page=page.child(div().flex().child(button("copy-sandbox",if self.copied{"Copied"}else{"Copy connection details"},palette,true,cx,|view,_,cx|{
                if let Some(manager)=&view.manager&&let Some(instance)=manager.snapshot().active{
                    cx.write_to_clipboard(ClipboardItem::new_string(format!("MYSQL_HOST=127.0.0.1\nMYSQL_PORT={}\nMYSQL_USER=sandbox\nMYSQL_PASSWORD={}\nCLICKHOUSE_HOST=127.0.0.1\nCLICKHOUSE_HTTP_PORT={}\nCLICKHOUSE_NATIVE_PORT={}\nCLICKHOUSE_USER=sandbox\nCLICKHOUSE_PASSWORD={}\nDOCKER_NETWORK={}_default\nDOCKER_MYSQL_HOST=mysql\nDOCKER_CLICKHOUSE_HOST=clickhouse\n",instance.ports[0],manager.store.password,instance.ports[1],instance.ports[2],manager.store.password,instance.project)));
                    view.copied=true;cx.notify();
                }
            })));
        } else {
            page=page.child(div().py_4().text_sm().child("Create MySQL and ClickHouse from the current local backup pair. Download both backups on the Backups page first."));
        }
        page.child(div().text_sm().text_color(rgb(palette.text_muted)).child("Databases stop when Sandbox Prepare closes. Stopping preserves data; Delete removes both database volumes."))
    }
    fn processes_page(&self, palette: Palette, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div()
                    .p_2()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_1()
                    .children(
                        ["All", "MySQL", "ClickHouse", "Restore"]
                            .into_iter()
                            .enumerate()
                            .map(|(index, label)| {
                                button(
                                    ("process-filter", index),
                                    label,
                                    palette,
                                    true,
                                    cx,
                                    move |view, _, cx| {
                                        view.filter = index;
                                        view.refresh_lines();
                                        cx.notify();
                                    },
                                )
                                .when(self.filter == index, |button| {
                                    button.bg(rgb(palette.element_selected))
                                })
                            }),
                    )
                    .child(div().flex_1())
                    .child(button(
                        "pause-process-logs",
                        if self.paused { "Resume" } else { "Pause" },
                        palette,
                        true,
                        cx,
                        |view, _, cx| {
                            view.paused = !view.paused;
                            if !view.paused {
                                view.refresh_lines();
                            }
                            cx.notify();
                        },
                    )),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(palette.text_muted))
                    .child(format!(
                        "{} records · container and restore output · newest at bottom",
                        self.lines.len()
                    )),
            )
            .when(self.lines.is_empty(), |page| {
                page.child(
                    div().p_4().text_sm().child(
                        "Container output will appear here when you create or start a sandbox.",
                    ),
                )
            })
            .child(
                list(
                    self.list.clone(),
                    cx.processor(move |view, index, _, _| {
                        view.log_row(index, palette).into_any_element()
                    }),
                )
                .w_full()
                .flex_1()
                .min_h_0(),
            )
    }
    fn modal(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let recreate = matches!(self.confirmation, Some(Confirmation::Recreate));
        div().absolute().inset_0().flex().items_center().justify_center().bg(gpui::black().opacity(0.3)).occlude()
            .on_key_down(cx.listener(|view,event:&gpui::KeyDownEvent,window,cx|{
                match event.keystroke.key.as_str(){"escape"=>{view.confirmation=None;cx.notify();},"tab"=>{view.modal_index=1-view.modal_index;view.modal_focus[view.modal_index].focus(window);},_=>return}cx.stop_propagation();
            }))
            .child(div().w(px(440.)).max_w_full().p_5().gap_4().flex().flex_col().rounded_md().border_1().border_color(rgb(palette.border)).bg(rgb(palette.editor_background)).shadow_lg()
                .child(div().text_lg().child(if recreate{"Recreate databases?"}else{"Delete sandbox data?"}))
                .child(div().text_sm().child(if recreate{"Both databases will be restored into new volumes. After validation succeeds, the previous databases and their local changes will be deleted."}else{"Both database volumes and their local changes will be permanently deleted. Downloaded backup archives are preserved."}))
                .child(div().flex().justify_end().gap_2()
                    .child(button("cancel-sandbox-confirmation","Cancel",palette,true,cx,|view,_,cx|{view.confirmation=None;cx.notify();}).track_focus(&self.modal_focus[0]))
                    .child(button("confirm-sandbox-action",if recreate{"Recreate"}else{"Delete"},palette,true,cx,move|view,_,cx|{view.confirmation=None;view.act(if recreate{Action::Create}else{Action::Delete},cx);}).track_focus(&self.modal_focus[1]).text_color(rgb(palette.error)))))
    }
}
impl Render for SandboxesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.theme.palette(window.appearance());
        div()
            .relative()
            .track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &crate::FocusNext, window, cx| {
                if view.confirmation.is_some() {
                    view.modal_index = 1 - view.modal_index;
                    view.modal_focus[view.modal_index].focus(window);
                } else {
                    window.focus_next();
                }
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|view, _: &crate::FocusPrevious, window, cx| {
                if view.confirmation.is_some() {
                    view.modal_index = 1 - view.modal_index;
                    view.modal_focus[view.modal_index].focus(window);
                } else {
                    window.focus_prev();
                }
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, window, cx| {
                if view.confirmation.is_some() && event.keystroke.key == "escape" {
                    view.confirmation = None;
                    view.focus.focus(window);
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .text_color(rgb(palette.text))
            .bg(rgb(palette.editor_background))
            .child(if self.processes {
                self.processes_page(palette, cx).into_any_element()
            } else {
                self.sandbox_page(palette, cx)
                    .id("sandbox-scroll")
                    .overflow_y_scroll()
                    .into_any_element()
            })
            .when(self.confirmation.is_some(), |page| {
                page.child(self.modal(palette, cx))
            })
    }
}
fn button(
    id: impl Into<gpui::ElementId>,
    label: &str,
    palette: Palette,
    enabled: bool,
    cx: &Context<SandboxesView>,
    action: impl Fn(&mut SandboxesView, &mut Window, &mut Context<SandboxesView>) + 'static,
) -> gpui::Stateful<Div> {
    let action = std::rc::Rc::new(action);
    let key = action.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(28.))
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(rgb(palette.border))
        .bg(rgb(palette.element_background))
        .text_sm()
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
                    key(view, window, cx);
                    cx.stop_propagation();
                }
            }),
        )
        .child(label.to_owned())
}
