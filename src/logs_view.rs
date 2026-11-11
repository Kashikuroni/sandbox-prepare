use crate::{
    logging::{self, Cursor, Entry, Filter, LogStore, Page},
    theme::{Palette, ThemeMode},
};
use gpui::{
    Context, Div, IntoElement, ListAlignment, ListState, Render, Stateful, Window, div, list,
    prelude::*, px, rgb,
};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Current,
    History,
}
pub struct LogsView {
    store: Arc<LogStore>,
    source: Source,
    theme: ThemeMode,
    filter: Filter,
    entries: Vec<Arc<Entry>>,
    list: ListState,
    revision: u64,
    history: Vec<Option<Cursor>>,
    next: Option<Cursor>,
    loading: bool,
    error: Option<String>,
    malformed: usize,
    request: u64,
    events: mpsc::Sender<(u64, Result<Page, String>)>,
    disk_error: Option<String>,
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
            history: vec![None],
            next: None,
            loading: false,
            error: None,
            malformed: 0,
            request: 0,
            events,
            disk_error: None,
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
    fn row(&self, index: usize, palette: Palette, cx: &mut Context<Self>) -> Div {
        let entry = self.entries[index].clone();
        let copy = entry.clone();
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .px_3()
            .py_3()
            .gap_1()
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if entry.level == "ERROR" {
                                palette.error
                            } else {
                                palette.text_muted
                            }))
                            .child(format!(
                                "{} · {} · {} · #{}",
                                entry.timestamp, entry.level, entry.category, entry.seq
                            )),
                    )
                    .child(button(
                        ("copy-log", index),
                        "Copy",
                        palette,
                        false,
                        cx,
                        move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.text()))
                        },
                    )),
            )
            .child(div().text_sm().child(entry.message.clone()))
            .children(entry.fields.iter().map(|(key, value)| {
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
                    .child(format!("{key}: {value}"))
            }))
    }
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
                    .h(px(44.))
                    .flex_shrink_0()
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(palette.border))
                    .child(button(
                        "logs-current",
                        "Current",
                        palette,
                        self.source == Source::Current,
                        cx,
                        |view, _, cx| view.source(Source::Current, cx),
                    ))
                    .child(button(
                        "logs-history",
                        "History",
                        palette,
                        self.source == Source::History,
                        cx,
                        |view, _, cx| view.source(Source::History, cx),
                    ))
                    .child(div().flex_1())
                    .child(button(
                        "refresh-logs",
                        "Refresh",
                        palette,
                        false,
                        cx,
                        |view, _, cx| {
                            if !view.loading {
                                view.refresh(cx)
                            }
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(palette.border))
                    .child(
                        div().flex().flex_wrap().gap_1().children(
                            [
                                "All", "startup", "ui", "settings", "storage", "s3", "download",
                                "panic",
                            ]
                            .into_iter()
                            .map(|category| {
                                let selected = self.filter.category.as_deref()
                                    == if category == "All" {
                                        None
                                    } else {
                                        Some(category)
                                    };
                                button(
                                    gpui::SharedString::from(format!("category-{category}")),
                                    category,
                                    palette,
                                    selected,
                                    cx,
                                    move |view, _, cx| {
                                        view.filter.category =
                                            (category != "All").then(|| category.into());
                                        view.refresh(cx);
                                    },
                                )
                            }),
                        ),
                    )
                    .child(
                        div().flex().flex_wrap().gap_1().children(
                            ["All levels", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
                                .into_iter()
                                .map(|level| {
                                    let selected = self.filter.level.as_deref()
                                        == if level == "All levels" {
                                            None
                                        } else {
                                            Some(level)
                                        };
                                    button(
                                        gpui::SharedString::from(format!("level-{level}")),
                                        level,
                                        palette,
                                        selected,
                                        cx,
                                        move |view, _, cx| {
                                            view.filter.level =
                                                (level != "All levels").then(|| level.into());
                                            view.refresh(cx);
                                        },
                                    )
                                }),
                        ),
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
    }
}
