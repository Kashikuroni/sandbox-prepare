use crate::{
    icons::Icon,
    logging::{self, Cursor, Entry, Filter, LogStore, Page},
    theme::{Palette, ThemeMode},
};
use gpui::{
    Context, Div, IntoElement, ListAlignment, ListState, Render, Stateful, Window, div, list,
    prelude::*, px, rgb,
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{Arc, mpsc},
    time::Duration,
};
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Current,
    History,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    Category,
    Level,
}
impl FilterKind {
    fn index(self) -> usize {
        if self == Self::Category { 0 } else { 1 }
    }
    fn options(self) -> &'static [&'static str] {
        match self {
            Self::Category => &[
                "All categories",
                "startup",
                "ui",
                "settings",
                "storage",
                "s3",
                "download",
                "sandbox",
                "panic",
            ],
            Self::Level => &["All levels", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
        }
    }
}
pub struct LogsView {
    store: Arc<LogStore>,
    source: Source,
    theme: ThemeMode,
    filter: Filter,
    entries: Vec<Arc<Entry>>,
    list: ListState,
    revision: u64,
    history_revision: u64,
    history: Vec<Option<Cursor>>,
    next: Option<Cursor>,
    loading: bool,
    error: Option<String>,
    malformed: usize,
    request: u64,
    events: mpsc::Sender<(u64, Result<Page, String>)>,
    disk_error: Option<String>,
    details: Option<Arc<Entry>>,
    copied: bool,
    copy_error: Option<String>,
    open_filter: Option<FilterKind>,
    menu_index: usize,
    menu_focus: gpui::FocusHandle,
    selector_focus: [gpui::FocusHandle; 2],
    source_focus: [gpui::FocusHandle; 2],
    selector_bounds: [Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>>; 2],
}
impl LogsView {
    pub fn new(theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        let (events, receiver) = mpsc::channel::<(u64, Result<Page, String>)>();
        let store = logging::store();
        cx.spawn(async move |view, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(200)).await;
                if view
                    .update(cx, |view, cx| {
                        let mut changed = false;
                        if view.history_revision != view.store.history_revision() {
                            view.history_revision = view.store.history_revision();
                            view.request += 1;
                            view.details = None;
                            view.entries.clear();
                            view.list.reset(0);
                            view.history = vec![None];
                            view.next = None;
                            if view.source == Source::History {
                                view.read_history(None, cx);
                            } else {
                                view.refresh_current();
                            }
                            changed = true;
                        }
                        for (request, result) in receiver.try_iter() {
                            if request != view.request {
                                continue;
                            }
                            view.loading = false;
                            match result {
                                Ok(page) => {
                                    view.entries = page.entries;
                                    view.next = page.next;
                                    view.malformed = page.malformed;
                                    view.list.reset(view.entries.len());
                                }
                                Err(error) => view.error = Some(error),
                            }
                            changed = true;
                        }
                        if view.source == Source::Current && view.revision != view.store.revision()
                        {
                            view.refresh_current();
                            changed = true;
                        }
                        let error = view.store.disk_error();
                        if view.disk_error != error {
                            view.disk_error = error;
                            changed = true;
                        }
                        if changed {
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
        let mut view = Self {
            store,
            source: Source::Current,
            theme,
            filter: Filter::default(),
            entries: vec![],
            list: ListState::new(0, ListAlignment::Bottom, px(400.)),
            revision: u64::MAX,
            history_revision: 0,
            history: vec![None],
            next: None,
            loading: false,
            error: None,
            malformed: 0,
            request: 0,
            events,
            disk_error: None,
            details: None,
            copied: false,
            copy_error: None,
            open_filter: None,
            menu_index: 0,
            menu_focus: cx.focus_handle(),
            selector_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            source_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            selector_bounds: std::array::from_fn(|_| Rc::new(Cell::new(None))),
        };
        view.refresh_current();
        view
    }
    fn refresh_current(&mut self) {
        let revision = self.store.revision();
        let entries: Vec<_> = self
            .store
            .snapshot()
            .into_iter()
            .filter(|entry| self.filter.matches(entry))
            .collect();
        let removed = if let Some(first) = entries.first() {
            self.entries
                .iter()
                .take_while(|entry| entry.seq < first.seq)
                .count()
        } else {
            self.entries.len()
        };
        let retained = self.entries.len() - removed;
        if entries.len() >= retained
            && self.entries[removed..]
                .iter()
                .zip(&entries[..retained])
                .all(|(a, b)| a.seq == b.seq)
        {
            self.list.splice(0..removed, 0);
            self.list
                .splice(retained..retained, entries.len() - retained);
        } else {
            self.list.reset(entries.len());
        }
        self.entries = entries;
        self.revision = revision;
    }

    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        self.remeasure(cx);
    }
    pub fn remeasure(&mut self, cx: &mut Context<Self>) {
        self.list.reset(self.entries.len());
        cx.notify();
    }
    fn read_history(&mut self, cursor: Option<Cursor>, cx: &mut Context<Self>) {
        self.request += 1;
        self.loading = true;
        self.error = None;
        self.next = None;
        let request = self.request;
        let events = self.events.clone();
        let store = self.store.clone();
        let filter = self.filter.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let _ = store.flush();
                let directory = store
                    .directory()
                    .ok_or_else(|| anyhow::anyhow!("Log directory is unavailable"))?;
                logging::history_page(&directory, cursor, &filter, 200)
            })()
            .map_err(|error: anyhow::Error| error.to_string());
            let _ = events.send((request, result));
        });
        cx.notify();
    }
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.source == Source::Current {
            self.refresh_current();
        } else {
            self.history = vec![None];
            self.read_history(None, cx);
        }
        cx.notify();
    }
    fn source(&mut self, source: Source, cx: &mut Context<Self>) {
        self.open_filter = None;
        self.source = source;
        self.request += 1;
        self.loading = false;
        self.error = None;
        self.entries.clear();
        self.list = ListState::new(
            0,
            if source == Source::Current {
                ListAlignment::Bottom
            } else {
                ListAlignment::Top
            },
            px(400.),
        );
        self.refresh(cx);
    }
    fn selected_filter(&self, kind: FilterKind) -> usize {
        let value = match kind {
            FilterKind::Category => &self.filter.category,
            FilterKind::Level => &self.filter.level,
        };
        value
            .as_ref()
            .and_then(|value| kind.options().iter().position(|option| *option == value))
            .unwrap_or(0)
    }
    fn close_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(kind) = self.open_filter.take() {
            self.selector_focus[kind.index()].focus(window);
        }
        cx.notify();
    }
    fn toggle_filter(&mut self, kind: FilterKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_filter == Some(kind) {
            self.close_filter(window, cx);
        } else {
            self.open_filter = Some(kind);
            self.menu_index = self.selected_filter(kind);
            self.menu_focus.focus(window);
            cx.notify();
        }
    }
    fn choose_filter(
        &mut self,
        kind: FilterKind,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = (index != 0).then(|| kind.options()[index].to_owned());
        match kind {
            FilterKind::Category => self.filter.category = value,
            FilterKind::Level => self.filter.level = value,
        }
        self.close_filter(window, cx);
        self.refresh(cx);
    }
    fn selector(&self, kind: FilterKind, palette: Palette, cx: &mut Context<Self>) -> Div {
        let bounds = self.selector_bounds[kind.index()].clone();
        let open = self.open_filter == Some(kind);
        let selected = self.selected_filter(kind);
        let label = kind.options()[selected];
        div()
            .relative()
            .min_w_0()
            .child(
                div()
                    .id(("log-selector", kind.index()))
                    .w_full()
                    .min_w_0()
                    .track_focus(&self.selector_focus[kind.index()])
                    .flex()
                    .items_center()
                    .h(px(28.))
                    .px_2()
                    .gap_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .cursor_pointer()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .when(open, |el| el.bg(rgb(palette.element_selected)))
                    .hover(|el| el.bg(rgb(palette.element_hover)))
                    .focus(|el| el.border_color(rgb(palette.border_focused)))
                    .on_click(
                        cx.listener(move |view, _, window, cx| {
                            view.toggle_filter(kind, window, cx)
                        }),
                    )
                    .on_key_down(cx.listener(
                        move |view, event: &gpui::KeyDownEvent, window, cx| {
                            if matches!(
                                event.keystroke.key.as_str(),
                                "enter" | "space" | "down" | "up"
                            ) {
                                view.toggle_filter(kind, window, cx);
                                cx.stop_propagation();
                            }
                        },
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .when(kind == FilterKind::Level && selected != 0, |el| {
                                el.font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(palette.log_level_color(label)))
                            })
                            .child(label),
                    )
                    .child(Icon::ChevronDown.render(palette.icon_muted).size(px(12.))),
            )
            .child(
                gpui::canvas(
                    move |rectangle, _, _| bounds.set(Some(rectangle)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when(open, |el| {
                el.child(
                    div().absolute().top_0().left_0().child(
                        gpui::deferred(
                            gpui::anchored()
                                .offset(gpui::point(px(0.), px(32.)))
                                .snap_to_window_with_margin(px(8.))
                                .child(self.filter_menu(kind, palette, cx)),
                        )
                        .with_priority(1),
                    ),
                )
            })
    }
    fn filter_menu(
        &self,
        kind: FilterKind,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        div()
            .id("log-filter-menu")
            .track_focus(&self.menu_focus)
            .occlude()
            .flex()
            .flex_col()
            .w(px(200.))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(palette.border))
            .shadow_md()
            .bg(rgb(palette.panel_background))
            .text_color(rgb(palette.text))
            .on_mouse_down_out(
                cx.listener(move |view, event: &gpui::MouseDownEvent, _, cx| {
                    if !view.selector_bounds[kind.index()]
                        .get()
                        .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        view.open_filter = None;
                        cx.notify();
                    }
                }),
            )
            .on_key_down(
                cx.listener(move |view, event: &gpui::KeyDownEvent, window, cx| {
                    let len = kind.options().len();
                    match event.keystroke.key.as_str() {
                        "escape" => view.close_filter(window, cx),
                        "down" => view.menu_index = (view.menu_index + 1) % len,
                        "up" => view.menu_index = (view.menu_index + len - 1) % len,
                        "home" => view.menu_index = 0,
                        "end" => view.menu_index = len - 1,
                        "enter" | "space" => view.choose_filter(kind, view.menu_index, window, cx),
                        "tab" => {
                            view.close_filter(window, cx);
                            return;
                        }
                        _ => return,
                    }
                    cx.notify();
                    cx.stop_propagation();
                }),
            )
            .children(kind.options().iter().enumerate().map(|(index, label)| {
                let selected = index == self.selected_filter(kind);
                div()
                    .id(("filter-option", index))
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(30.))
                    .px_2()
                    .rounded_sm()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(rgb(palette.element_selected)))
                    .when(!selected && index == self.menu_index, |el| {
                        el.bg(rgb(palette.element_hover))
                    })
                    .hover(|el| {
                        el.bg(rgb(if selected {
                            palette.element_selected
                        } else {
                            palette.element_hover
                        }))
                    })
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.choose_filter(kind, index, window, cx)
                    }))
                    .child(
                        div()
                            .when(kind == FilterKind::Level && index != 0, |el| {
                                el.font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(palette.log_level_color(label)))
                            })
                            .child(*label),
                    )
                    .when(selected, |el| {
                        el.child(Icon::Check.render(palette.icon_accent).size(px(14.)))
                    })
            }))
    }
    fn source_tab(
        &self,
        source: Source,
        index: usize,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let selected = self.source == source;
        div()
            .id(("log-source", index))
            .track_focus(&self.source_focus[index])
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .justify_center()
            .text_sm()
            .cursor_pointer()
            .text_color(rgb(if selected {
                palette.text
            } else {
                palette.text_muted
            }))
            .bg(rgb(if selected {
                palette.panel_background
            } else {
                palette.editor_background
            }))
            .border_b_1()
            .border_color(if selected {
                gpui::transparent_black()
            } else {
                rgb(palette.border).into()
            })
            .focus(|el| el.text_color(rgb(palette.text)))
            .on_click(cx.listener(move |view, _, window, cx| {
                view.source_focus[index].focus(window);
                view.source(source, cx);
            }))
            .on_key_down(
                cx.listener(move |view, event: &gpui::KeyDownEvent, window, cx| {
                    let selected = match event.keystroke.key.as_str() {
                        "left" | "home" => 0,
                        "right" | "end" => 1,
                        "enter" | "space" => index,
                        _ => return,
                    };
                    view.source_focus[selected].focus(window);
                    view.source(
                        if selected == 0 {
                            Source::Current
                        } else {
                            Source::History
                        },
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
            .child(if source == Source::Current {
                "Current"
            } else {
                "History"
            })
    }
    fn row(&self, index: usize, palette: Palette, cx: &mut Context<Self>) -> Div {
        let entry = self.entries[index].clone();
        let details = entry.clone();
        div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(rgb(palette.text_muted))
                            .child(display_timestamp(&entry.timestamp))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(palette.log_level_color(&entry.level)))
                                    .child(entry.level.clone()),
                            ),
                    )
                    .child(div().flex_1())
                    .child(button(
                        ("log-details", index),
                        "Details",
                        palette,
                        false,
                        cx,
                        move |view, _, cx| {
                            view.details = Some(details.clone());
                            view.copied = false;
                            view.copy_error = None;
                            cx.notify();
                        },
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .w_full()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(log_title(&entry.message).to_owned()),
            )
    }
    fn render_details(&self, entry: Arc<Entry>, palette: Palette, cx: &mut Context<Self>) -> Div {
        let copy = entry.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(rgb(palette.panel_background))
            .text_color(rgb(palette.text))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .h(px(44.))
                    .px_2()
                    .border_b_1()
                    .border_color(rgb(palette.border))
                    .child(button(
                        "back-to-logs",
                        "Back",
                        palette,
                        false,
                        cx,
                        |view, _, cx| {
                            view.details = None;
                            cx.notify();
                        },
                    ))
                    .child(div().flex_1())
                    .child(button(
                        "copy-log-json",
                        if self.copied { "Copied" } else { "Copy" },
                        palette,
                        false,
                        cx,
                        move |view, _, cx| {
                            match copy.json() {
                                Ok(json) => {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(json));
                                    view.copied = true;
                                    view.copy_error = None;
                                }
                                Err(error) => {
                                    view.copy_error = Some(format!("Could not copy JSON: {error}"))
                                }
                            };
                            cx.notify();
                        },
                    )),
            )
            .child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "log-detail-{}-{}",
                        entry.session_id, entry.seq
                    )))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .p_3()
                    .gap_4()
                    .children(self.copy_error.as_ref().map(|error| {
                        div()
                            .text_sm()
                            .text_color(rgb(palette.error))
                            .child(error.clone())
                    }))
                    .child(detail_field("Message", &entry.message, palette))
                    .child(detail_field("Level", &entry.level, palette))
                    .child(detail_field(
                        "Date (local time)",
                        &display_timestamp(&entry.timestamp),
                        palette,
                    ))
                    .child(detail_field("Timestamp (UTC)", &entry.timestamp, palette))
                    .child(detail_field("Category", &entry.category, palette))
                    .child(detail_field("Session", &entry.session_id, palette))
                    .child(detail_field("Sequence", &entry.seq.to_string(), palette))
                    .children(
                        entry
                            .fields
                            .iter()
                            .map(|(key, value)| detail_field(key, value, palette)),
                    ),
            )
    }
}
fn detail_field(label: &str, value: &str, palette: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(palette.text_muted))
                .child(label.to_owned()),
        )
        .child(div().text_sm().w_full().child(value.to_owned()))
}
fn log_title(message: &str) -> &str {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("(empty message)")
}
fn display_timestamp(timestamp: &str) -> String {
    timestamp_in_zone(timestamp, &chrono::Local)
}
fn timestamp_in_zone<T: chrono::TimeZone>(timestamp: &str, zone: &T) -> String
where
    T::Offset: std::fmt::Display,
{
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|date| {
            date.with_timezone(zone)
                .format("%d.%m.%Y %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.to_owned())
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: &str,
    palette: Palette,
    selected: bool,
    cx: &Context<LogsView>,
    action: impl Fn(&mut LogsView, &mut Window, &mut Context<LogsView>) + 'static,
) -> Stateful<Div> {
    let action = std::rc::Rc::new(action);
    let key_action = action.clone();
    div()
        .id(id)
        .tab_index(0)
        .flex_shrink_0()
        .px_2()
        .py_1()
        .text_xs()
        .rounded_sm()
        .border_1()
        .border_color(gpui::transparent_black())
        .text_color(rgb(if selected {
            palette.text
        } else {
            palette.text_muted
        }))
        .when(selected, |el| el.bg(rgb(palette.element_selected)))
        .hover(|el| el.bg(rgb(palette.element_hover)))
        .focus(|el| el.border_color(rgb(palette.border_focused)))
        .cursor_pointer()
        .on_click(cx.listener(move |view, _, window, cx| action(view, window, cx)))
        .on_key_down(
            cx.listener(move |view, event: &gpui::KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    key_action(view, window, cx);
                    cx.stop_propagation();
                }
            }),
        )
        .child(label.to_owned())
}
impl Render for LogsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.theme.palette(window.appearance());
        if let Some(entry) = self.details.clone() {
            return self.render_details(entry, palette, cx).into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(rgb(palette.panel_background))
            .text_color(rgb(palette.text))
            .child(
                div()
                    .flex()
                    .w_full()
                    .h(px(32.))
                    .flex_shrink_0()
                    .child(self.source_tab(Source::Current, 0, palette, cx))
                    .child(
                        div()
                            .w(px(1.))
                            .h_full()
                            .flex_shrink_0()
                            .bg(rgb(palette.border)),
                    )
                    .child(self.source_tab(Source::History, 1, palette, cx)),
            )
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .h(px(40.))
                    .flex_shrink_0()
                    .px_2()
                    .gap_1()
                    .child(self.selector(FilterKind::Category, palette, cx))
                    .child(self.selector(FilterKind::Level, palette, cx))
                    .child(div().flex_1())
                    .child(
                        button("refresh-logs", "", palette, false, cx, |view, _, cx| {
                            if !view.loading {
                                view.refresh(cx)
                            }
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(28.))
                        .h(px(28.))
                        .p_0()
                        .child(Icon::Refresh.render(if self.loading {
                            palette.icon_muted
                        } else {
                            palette.icon
                        }))
                        .tooltip(move |_, cx| cx.new(|_| RefreshTooltip(palette)).into()),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(palette.text_muted))
                    .child(if self.source == Source::Current {
                        format!(
                            "{} records · current session · {} older records in History",
                            self.entries.len(),
                            self.store.evicted()
                        )
                    } else {
                        format!(
                            "{} records · page {} · oldest first",
                            self.entries.len(),
                            self.history.len()
                        )
                    }),
            )
            .children(
                self.store
                    .disk_error()
                    .into_iter()
                    .chain(self.error.clone())
                    .map(|error| {
                        div()
                            .px_3()
                            .text_sm()
                            .text_color(rgb(palette.error))
                            .child(error)
                    }),
            )
            .when(self.malformed > 0 && self.source == Source::History, |el| {
                el.child(
                    div()
                        .px_3()
                        .text_xs()
                        .child(format!("Skipped {} malformed JSON lines", self.malformed)),
                )
            })
            .when(self.loading, |el| {
                el.child(div().p_3().text_sm().child("Reading log files…"))
            })
            .when(self.entries.is_empty() && !self.loading, |el| {
                el.child(div().p_3().text_sm().child("No matching log records."))
            })
            .child(
                list(
                    self.list.clone(),
                    cx.processor(move |view, index, _, cx| {
                        view.row(index, palette, cx).into_any_element()
                    }),
                )
                .w_full()
                .flex_1()
                .min_h_0(),
            )
            .when(self.source == Source::History, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_between()
                        .p_2()
                        .when(self.history.len() > 1 && !self.loading, |el| {
                            el.child(button(
                                "logs-previous",
                                "Previous",
                                palette,
                                false,
                                cx,
                                |view, _, cx| {
                                    view.history.pop();
                                    view.read_history(view.history.last().cloned().flatten(), cx);
                                },
                            ))
                        })
                        .when(self.next.is_some() && !self.loading, |el| {
                            el.child(button(
                                "logs-next",
                                "Next",
                                palette,
                                false,
                                cx,
                                |view, _, cx| {
                                    if let Some(cursor) = view.next.clone() {
                                        view.history.push(Some(cursor.clone()));
                                        view.read_history(Some(cursor), cx);
                                    }
                                },
                            ))
                        }),
                )
            })
            .into_any_element()
    }
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
            .child("Refresh logs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates_use_the_selected_zone_and_requested_format() {
        let zone = chrono::FixedOffset::east_opt(3 * 3600).unwrap();
        assert_eq!(
            timestamp_in_zone("2026-11-11T06:59:35.123Z", &zone),
            "11.11.2026 09:59:35"
        );
        assert_eq!(
            timestamp_in_zone("2026-12-31T23:59:59Z", &zone),
            "01.01.2027 02:59:59"
        );
        assert_eq!(timestamp_in_zone("unknown", &zone), "unknown");
    }
    #[test]
    fn details_json_preserves_all_information_and_multiline_messages() -> anyhow::Result<()> {
        let entry = Entry {
            session_id: "session-1".into(),
            seq: 42,
            timestamp: "2026-11-11T06:59:35Z".into(),
            level: "ERROR".into(),
            category: "s3".into(),
            message: "Connection failed\nFull diagnostic details".into(),
            fields: std::collections::BTreeMap::from([("reason".into(), "unavailable".into())]),
        };
        assert_eq!(log_title(&entry.message), "Connection failed");
        let copied: Entry = serde_json::from_str(&entry.json()?)?;
        assert_eq!(copied.message, entry.message);
        assert_eq!(copied.session_id, entry.session_id);
        assert_eq!(copied.seq, entry.seq);
        assert_eq!(copied.fields, entry.fields);
        Ok(())
    }
}
