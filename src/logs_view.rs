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
    details: Option<Arc<Entry>>,
    copied: bool,
    copy_error: Option<String>,
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
            details: None,
            copied: false,
            copy_error: None,
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
                            .min_w_0()
                            .text_xs()
                            .text_color(rgb(palette.text_muted))
                            .child(format!(
                                "{} {}",
                                entry.level,
                                display_timestamp(&entry.timestamp)
                            )),
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
