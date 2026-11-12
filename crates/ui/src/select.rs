//! Searchable, keyboard-operable GPUI selector. Values are stable IDs, labels are presentation.
use crate::{
    icons::Icon,
    text_input::{Edited, TextInput},
    theme::{ThemeMode, ThemePalette},
};
use gpui::{prelude::*, *};
#[derive(Clone)]
pub struct OptionItem {
    pub value: String,
    pub label: String,
}
impl OptionItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}
pub struct Selected(pub String);
pub struct Select {
    options: Vec<OptionItem>,
    value: String,
    placeholder: String,
    theme: ThemeMode,
    search: Entity<TextInput>,
    focus: FocusHandle,
    open: bool,
    index: usize,
    scroll: ScrollHandle,
    bounds: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
}
impl EventEmitter<Selected> for Select {}
impl Select {
    pub fn new(placeholder: &str, theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::picker_search(theme, cx));
        cx.subscribe(&search, |this, _, _: &Edited, cx| {
            this.index = 0;
            this.scroll.scroll_to_item(0);
            cx.notify();
        })
        .detach();
        Self {
            options: vec![],
            value: String::new(),
            placeholder: placeholder.into(),
            theme,
            search,
            focus: cx.focus_handle().tab_stop(true),
            open: false,
            index: 0,
            scroll: ScrollHandle::new(),
            bounds: Default::default(),
        }
    }
    pub fn set(&mut self, options: Vec<OptionItem>, value: String, cx: &mut Context<Self>) {
        self.options = options;
        self.value = value;
        cx.notify();
    }
    pub fn value(&self) -> &str {
        &self.value
    }
    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        self.search
            .update(cx, |input, cx| input.set_theme(theme, cx));
        cx.notify();
    }
    fn filtered(&self, cx: &Context<Self>) -> Vec<OptionItem> {
        let query = self.search.read(cx).value().to_lowercase();
        self.options
            .iter()
            .filter(|item| item.label.to_lowercase().contains(&query))
            .cloned()
            .collect()
    }
    fn choose(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        self.value = value.clone();
        self.open = false;
        self.focus.focus(window);
        cx.emit(Selected(value));
        cx.notify();
    }
    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = !self.open;
        self.index = 0;
        if self.open {
            self.search
                .update(cx, |input, cx| input.set_value(String::new(), cx));
            self.index = self
                .options
                .iter()
                .position(|item| item.value == self.value)
                .unwrap_or(0);
            self.scroll.scroll_to_item(self.index);
            self.search.read(cx).focus(window);
        } else {
            self.focus.focus(window);
        }
        cx.notify();
    }
}
impl Render for Select {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.theme.palette(window.appearance());
        let label = self
            .options
            .iter()
            .find(|item| item.value == self.value)
            .map(|item| item.label.clone())
            .unwrap_or_else(|| self.placeholder.clone());
        let options = self.filtered(cx);
        let menu = div()
            .id("select-menu")
            .occlude()
            .w(px(320.))
            .max_w(window.viewport_size().width - px(16.))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.panel_background))
            .shadow_md()
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if this
                    .bounds
                    .get()
                    .is_some_and(|bounds| bounds.contains(&event.position))
                {
                    return;
                }
                this.open = false;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let options = this.filtered(cx);
                let len = options.len();
                match event.keystroke.key.as_str() {
                    "escape" => {
                        this.open = false;
                        this.focus.focus(window);
                    }
                    "down" if len > 0 => this.index = (this.index + 1) % len,
                    "up" if len > 0 => this.index = (this.index + len - 1) % len,
                    "enter" if len > 0 => {
                        this.choose(options[this.index.min(len - 1)].value.clone(), window, cx)
                    }
                    "tab" => {
                        this.open = false;
                        cx.notify();
                        return;
                    }
                    _ => return,
                }
                this.scroll.scroll_to_item(this.index);
                cx.stop_propagation();
                cx.notify();
            }))
            .child(self.search.clone())
            .child(
                div()
                    .id("select-options")
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(options.iter().enumerate().map(|(index, item)| {
                        let value = item.value.clone();
                        let selected = self.value == item.value;
                        div()
                            .id(("option", index))
                            .h(px(28.))
                            .px_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(rgb(p.element_selected)))
                            .when(!selected && index == self.index, |d| {
                                d.bg(rgb(p.element_hover))
                            })
                            .hover(|d| d.bg(rgb(p.element_hover)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose(value.clone(), window, cx)
                            }))
                            .child(div().min_w_0().truncate().child(item.label.clone()))
                            .when(selected, |d| d.child(Icon::Check.render(p.icon_accent)))
                    }))
                    .when(options.is_empty(), |d| {
                        d.child(
                            div()
                                .p_2()
                                .text_color(rgb(p.text_muted))
                                .child("No matches"),
                        )
                    }),
            );
        let bounds = self.bounds.clone();
        div()
            .relative()
            .min_w_0()
            .w_full()
            .text_sm()
            .text_color(rgb(p.text))
            .child(
                div()
                    .id("select-trigger")
                    .track_focus(&self.focus)
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(p.border))
                    .focus(|d| d.border_color(rgb(p.border_focused)))
                    .hover(|d| d.bg(rgb(p.element_hover)))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle(window, cx)))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space" | "down") {
                            this.toggle(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(div().min_w_0().truncate().child(label))
                    .child(Icon::ChevronDown.render(p.icon_muted)),
            )
            .child(
                canvas(
                    move |rectangle, _, _| bounds.set(Some(rectangle)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when(self.open, |d| {
                d.child(
                    div().absolute().top_0().left_0().child(
                        deferred(
                            anchored()
                                .offset(point(px(0.), px(32.)))
                                .snap_to_window_with_margin(px(8.))
                                .child(menu),
                        )
                        .with_priority(2),
                    ),
                )
            })
    }
}
