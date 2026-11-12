//! Borderless titlebar pickers with grouped, searchable GPUI menus.
use crate::titlebar_style as titlebar;
use crate::{
    icons::Icon,
    text_input::{Edited, TextInput},
    theme::{ThemeMode, ThemePalette},
};
use gpui::{prelude::*, *};
#[derive(Clone, PartialEq, Eq)]
pub struct ContextItem {
    pub value: String,
    pub label: String,
    pub group: String,
    pub detail: String,
    pub status: String,
}
impl ContextItem {
    pub fn new(
        value: impl Into<String>,
        label: impl Into<String>,
        group: impl Into<String>,
    ) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            group: group.into(),
            detail: String::new(),
            status: String::new(),
        }
    }
    pub fn detail(mut self, value: impl Into<String>) -> Self {
        self.detail = value.into();
        self
    }
    pub fn status(mut self, value: impl Into<String>) -> Self {
        self.status = value.into();
        self
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct ContextAction {
    pub id: &'static str,
    pub label: &'static str,
}
pub enum PickerEvent {
    Opened,
    Selected(String),
    Action(&'static str),
    Dismissed(Point<Pixels>),
}
pub struct ContextPicker {
    items: Vec<ContextItem>,
    actions: Vec<ContextAction>,
    value: String,
    label: String,
    icon: Option<Icon>,
    theme: ThemeMode,
    search: Entity<TextInput>,
    focus: FocusHandle,
    open: bool,
    index: usize,
    scroll: ScrollHandle,
    bounds: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    message: Option<String>,
    submenu: bool,
    submenu_selected: bool,
}
impl EventEmitter<PickerEvent> for ContextPicker {}
impl ContextPicker {
    pub fn new(
        label: &str,
        search_label: &str,
        icon: Option<Icon>,
        theme: ThemeMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| TextInput::context_search(search_label, theme, cx));
        cx.subscribe(&search, |this, _, _: &Edited, cx| {
            this.index = 0;
            this.scroll.scroll_to_item(0);
            cx.notify();
        })
        .detach();
        Self {
            items: vec![],
            actions: vec![],
            value: String::new(),
            label: label.into(),
            icon,
            theme,
            search,
            focus: cx.focus_handle().tab_stop(true),
            open: false,
            index: 0,
            scroll: ScrollHandle::new(),
            bounds: Default::default(),
            message: None,
            submenu: false,
            submenu_selected: false,
        }
    }
    pub fn as_submenu(&mut self) {
        self.submenu = true;
    }
    pub fn set_submenu_selected(&mut self, selected: bool, cx: &mut Context<Self>) {
        if self.submenu_selected != selected {
            self.submenu_selected = selected;
            cx.notify();
        }
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn set(
        &mut self,
        items: Vec<ContextItem>,
        value: String,
        label: String,
        cx: &mut Context<Self>,
    ) {
        if self.items == items && self.value == value && self.label == label {
            return;
        }
        let old = self.entries(cx);
        let active = old.get(self.index).map(|item| item.value.clone());
        let action = self
            .index
            .checked_sub(old.len())
            .and_then(|index| self.actions.get(index))
            .map(|action| action.id);
        self.items = items;
        self.value = value;
        self.label = label;
        let entries = self.entries(cx);
        self.index = active
            .and_then(|value| entries.iter().position(|item| item.value == value))
            .or_else(|| {
                action.and_then(|id| {
                    self.actions
                        .iter()
                        .position(|action| action.id == id)
                        .map(|index| entries.len() + index)
                })
            })
            .unwrap_or_else(|| self.index.min(self.total(cx).saturating_sub(1)));
        cx.notify();
    }
    pub fn actions(&mut self, actions: Vec<ContextAction>, cx: &mut Context<Self>) {
        if self.actions != actions {
            self.actions = actions;
            cx.notify();
        }
    }
    pub fn message(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        if self.message != message {
            self.message = message;
            cx.notify();
        }
    }
    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        self.search
            .update(cx, |input, cx| input.set_theme(theme, cx));
        cx.notify();
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            cx.notify();
        }
    }
    fn entries(&self, cx: &Context<Self>) -> Vec<ContextItem> {
        let query = self.search.read(cx).value().to_lowercase();
        self.items
            .iter()
            .filter(|item| {
                item.label.to_lowercase().contains(&query)
                    || item.detail.to_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }
    pub fn query(&self, cx: &App) -> String {
        self.search.read(cx).value()
    }
    fn total(&self, cx: &Context<Self>) -> usize {
        self.entries(cx).len() + self.actions.len()
    }
    fn reveal(&self, cx: &Context<Self>) {
        let items = self.entries(cx);
        let index = self.index.min(items.len().saturating_sub(1));
        let headings = items
            .iter()
            .take(index + 1)
            .enumerate()
            .filter(|(i, item)| {
                !item.group.is_empty() && (*i == 0 || items[*i - 1].group != item.group)
            })
            .count();
        if !items.is_empty() {
            self.scroll.scroll_to_item(index + headings);
        }
    }
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            self.toggle(window, cx);
        }
    }
    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.focus.focus(window);
        } else {
            self.open = true;
            self.search
                .update(cx, |input, cx| input.set_value(String::new(), cx));
            self.index = self
                .items
                .iter()
                .position(|item| item.value == self.value)
                .unwrap_or(0);
            self.reveal(cx);
            self.search.read(cx).focus(window);
            cx.emit(PickerEvent::Opened);
        }
        cx.notify();
    }
    fn choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.entries(cx);
        let event = if let Some(item) = items.get(index) {
            Some(PickerEvent::Selected(item.value.clone()))
        } else {
            self.actions
                .get(index.saturating_sub(items.len()))
                .map(|a| PickerEvent::Action(a.id))
        };
        if let Some(event) = event {
            self.open = false;
            self.focus.focus(window);
            cx.emit(event);
            cx.notify();
        }
    }
}
impl ContextPicker {
    fn render_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let p = self.theme.palette(window.appearance());
        let entries = self.entries(cx);
        let max_menu_height = (window.viewport_size().height - px(48.)).max(px(0.));
        let footer_height = if self.actions.is_empty() {
            px(0.)
        } else {
            px(self.actions.len() as f32 * 30. + 10.)
        };
        let message_height = if self.message.is_some() {
            px(64.)
        } else {
            px(0.)
        };
        let options_height = (max_menu_height - px(40.) - footer_height - message_height)
            .max(px(0.))
            .min(px(300.));
        let count = entries.len();
        let mut rows = vec![];
        let mut group = String::new();
        for (index, item) in entries.iter().enumerate() {
            if !item.group.is_empty() && (index == 0 || group != item.group) {
                rows.push(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .text_size(px(12.))
                        .text_color(rgb(p.text_muted))
                        .child(item.group.clone())
                        .into_any_element(),
                );
                group = item.group.clone();
            }
            let selected = item.value == self.value;
            rows.push(
                div()
                    .id(("context-option", index))
                    .min_h(px(30.))
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(p.element_selected)))
                    .when(!selected && index == self.index, |d| {
                        d.bg(rgb(p.element_hover))
                    })
                    .hover(|d| d.bg(rgb(p.element_hover)))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose(index, window, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().min_w_0().truncate().child(item.label.clone()))
                                    .when(selected, |d| {
                                        d.child(Icon::Check.render(p.icon_accent).size(px(12.)))
                                    }),
                            )
                            .when(!item.detail.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(p.text_muted))
                                        .truncate()
                                        .child(item.detail.clone()),
                                )
                            }),
                    )
                    .when(!item.status.is_empty(), |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(12.))
                                .text_color(rgb(p.text_muted))
                                .child(item.status.clone()),
                        )
                    })
                    .into_any_element(),
            );
        }
        let menu = div()
            .id("context-menu")
            .occlude()
            .flex()
            .flex_col()
            .w(px(320.))
            .max_w(window.viewport_size().width - px(16.))
            .max_h(max_menu_height)
            .min_h_0()
            .rounded_md()
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.panel_background))
            .shadow_md()
            .text_size(px(13.))
            .text_color(rgb(p.text))
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if !this
                    .bounds
                    .get()
                    .is_some_and(|b| b.contains(&event.position))
                {
                    this.close(cx);
                    cx.emit(PickerEvent::Dismissed(event.position));
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let len = this.total(cx);
                match event.keystroke.key.as_str() {
                    "left" if this.submenu => {
                        this.open = false;
                        this.focus.focus(window);
                    }
                    "escape" => {
                        this.open = false;
                        this.focus.focus(window);
                    }
                    "down" if len > 0 => this.index = (this.index + 1) % len,
                    "up" if len > 0 => this.index = (this.index + len - 1) % len,
                    "enter" if len > 0 => this.choose(this.index, window, cx),
                    "tab" => {
                        this.open = false;
                        cx.notify();
                        return;
                    }
                    _ => return,
                }
                this.reveal(cx);
                cx.notify();
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(rgb(p.border))
                    .child(self.search.clone()),
            )
            .when_some(self.message.clone(), |d, message| {
                d.child(
                    div()
                        .id("context-message")
                        .flex_shrink_0()
                        .max_h(px(64.))
                        .overflow_y_scroll()
                        .px_3()
                        .py_2()
                        .text_size(px(13.))
                        .text_color(rgb(p.text_muted))
                        .child(message),
                )
            })
            .child(
                div()
                    .id("context-options")
                    .min_h_0()
                    .max_h(options_height)
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_1()
                    .children(rows)
                    .when(entries.is_empty() && self.message.is_none(), |d| {
                        d.child(
                            div()
                                .px_2()
                                .py_2()
                                .text_color(rgb(p.text_muted))
                                .child("No matches"),
                        )
                    }),
            )
            .when(!self.actions.is_empty(), |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .p_1()
                        .border_t_1()
                        .border_color(rgb(p.border))
                        .children(self.actions.iter().enumerate().map(|(i, action)| {
                            let index = count + i;
                            div()
                                .id(("context-action", i))
                                .h(px(30.))
                                .px_2()
                                .flex()
                                .items_center()
                                .rounded_sm()
                                .cursor_pointer()
                                .when(index == self.index, |d| d.bg(rgb(p.element_hover)))
                                .hover(|d| d.bg(rgb(p.element_hover)))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.choose(index, window, cx)
                                }))
                                .child(action.label)
                        })),
                )
            });
        menu
    }
    /// Render at the owning menu's overlay layer: GPUI cannot defer a draw inside another deferred draw.
    pub fn submenu_popup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.submenu || !self.open {
            return None;
        }
        let bounds = self.bounds.get()?;
        let x = if window.viewport_size().width - bounds.right() < px(328.) {
            bounds.left() - px(326.)
        } else {
            bounds.right() + px(6.)
        };
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .child(
                    deferred(
                        anchored()
                            .position(point(x, bounds.top()))
                            .snap_to_window_with_margin(px(8.))
                            .child(self.render_menu(window, cx)),
                    )
                    .with_priority(4),
                )
                .into_any_element(),
        )
    }
}
impl Render for ContextPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.theme.palette(window.appearance());
        let menu = if self.submenu {
            None
        } else {
            Some(self.render_menu(window, cx))
        };
        let bounds = self.bounds.clone();
        let offset = point(px(0.), px(titlebar::BUTTON_HEIGHT + 4.));
        div()
            .relative()
            .min_w_0()
            .when(self.submenu, |d| d.w_full())
            .font_family(titlebar::FONT_FAMILY)
            .text_size(px(titlebar::LABEL_SIZE))
            .text_color(rgb(if self.icon.is_some() && !self.submenu {
                p.text_muted
            } else {
                p.text
            }))
            .child(
                div()
                    .id("context-trigger")
                    .track_focus(&self.focus)
                    .h(px(if self.submenu {
                        30.
                    } else {
                        titlebar::BUTTON_HEIGHT
                    }))
                    .when(self.submenu, |d| d.w_full().text_size(px(13.)))
                    .px(px(titlebar::BUTTON_PADDING))
                    .flex()
                    .items_center()
                    .gap(px(titlebar::ICON_GAP))
                    .min_w_0()
                    .rounded_sm()
                    .cursor_pointer()
                    .focus(|d| d.bg(rgb(p.element_hover)))
                    .when(self.open, |d| {
                        d.bg(rgb(p.border_focused)).text_color(rgb(p.icon_accent))
                    })
                    .when(!self.open, |d| d.hover(|d| d.bg(rgb(p.element_hover))))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx)))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space" | "down") {
                            this.toggle(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .when_some(self.icon, |d, icon| {
                        d.child(
                            icon.render(if self.open {
                                p.icon_accent
                            } else {
                                p.icon_muted
                            })
                            .size(px(titlebar::ICON_SIZE)),
                        )
                    })
                    .child(
                        div()
                            .min_w_0()
                            .when(self.submenu, |d| d.flex_1())
                            .truncate()
                            .child(self.label.clone()),
                    )
                    .when(self.submenu_selected, |d| {
                        d.child(Icon::Check.render(p.icon_accent).size(px(12.)))
                    })
                    .when(self.submenu, |d| {
                        d.child(
                            Icon::ChevronDown
                                .render(p.icon_muted)
                                .size(px(12.))
                                .with_transformation(Transformation::rotate(radians(
                                    -std::f32::consts::FRAC_PI_2,
                                ))),
                        )
                    }),
            )
            .child(
                canvas(
                    move |rectangle, _, _| bounds.set(Some(rectangle)),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .when(self.open && !self.submenu, |d| {
                d.child(
                    div().absolute().top_0().left_0().child(
                        deferred(
                            anchored()
                                .offset(offset)
                                .snap_to_window_with_margin(px(8.))
                                .child(menu.expect("Standalone picker menu")),
                        )
                        .with_priority(3),
                    ),
                )
            })
    }
}
