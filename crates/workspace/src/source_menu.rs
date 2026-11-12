//! A working copy is the sandbox context; source creation stays in the titlebar.
use crate::{
    icons::Icon,
    theme::{ThemeMode, ThemePalette},
};
use gpui::{prelude::*, *};
use sandbox_core::models::{SandboxId, SourceMode};
use sandbox_ui::{
    context_picker::{ContextAction, ContextItem, ContextPicker, PickerEvent},
    titlebar_style as style,
};
use std::{cell::Cell, path::PathBuf, rc::Rc};

pub enum SourceEvent {
    Opened,
    Open(SourceMode),
    NewBranch(String),
    Legacy(SandboxId),
    Configure,
    Cancel,
}
impl EventEmitter<SourceEvent> for SourceMenu {}

pub struct SourceMenu {
    theme: ThemeMode,
    open: bool,
    return_focus: bool,
    focus: FocusHandle,
    index: usize,
    trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    menu_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    branches: Entity<ContextPicker>,
    worktrees: Entity<ContextPicker>,
    label: String,
    mode: Option<SourceMode>,
    message: Option<String>,
    status: String,
    directory: String,
    available: bool,
    busy: bool,
    legacy: Vec<(SandboxId, String)>,
    preview_submenu: Option<usize>,
}
impl SourceMenu {
    pub fn new(theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        let branches = cx.new(|cx| {
            ContextPicker::new(
                "Isolated Worktree",
                "Choose a free branch or type a new name…",
                Some(Icon::Branch),
                theme,
                cx,
            )
        });
        branches.update(cx, |p, _| p.as_submenu());
        cx.observe(&branches, |_, _, cx| cx.notify()).detach();
        let worktrees = cx.new(|cx| {
            ContextPicker::new(
                "Existing Git Worktree",
                "Search worktrees…",
                Some(Icon::Worktree),
                theme,
                cx,
            )
        });
        worktrees.update(cx, |p, _| p.as_submenu());
        cx.observe(&worktrees, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&branches, |this, _, event: &PickerEvent, cx| match event {
            PickerEvent::Opened => {
                this.index = 0;
                this.worktrees.update(cx, |p, cx| p.close(cx));
            }
            PickerEvent::Selected(branch) => {
                this.finish(cx);
                cx.emit(SourceEvent::Open(SourceMode::ManagedWorktree {
                    branch: branch.clone(),
                }));
            }
            PickerEvent::Action("new-branch") => {
                let branch = this.branches.read(cx).query(cx);
                this.finish(cx);
                cx.emit(SourceEvent::NewBranch(branch));
            }
            PickerEvent::Dismissed(position) => this.dismiss(*position, cx),
            _ => {}
        })
        .detach();
        cx.subscribe(&worktrees, |this, _, event: &PickerEvent, cx| match event {
            PickerEvent::Opened => {
                this.index = 2;
                this.branches.update(cx, |p, cx| p.close(cx));
            }
            PickerEvent::Selected(path) => {
                this.finish(cx);
                cx.emit(SourceEvent::Open(SourceMode::ExistingWorktree {
                    directory: PathBuf::from(path),
                }));
            }
            PickerEvent::Dismissed(position) => this.dismiss(*position, cx),
            _ => {}
        })
        .detach();
        Self {
            theme,
            open: false,
            return_focus: false,
            focus: cx.focus_handle().tab_stop(true),
            index: 0,
            trigger_bounds: Default::default(),
            menu_bounds: Default::default(),
            branches,
            worktrees,
            label: "Choose working copy".into(),
            mode: None,
            message: None,
            status: String::new(),
            directory: String::new(),
            available: false,
            busy: false,
            legacy: vec![],
            preview_submenu: if cfg!(debug_assertions)
                && std::env::var_os("SANDBOX_PREPARE_TEST_DATA_DIR").is_some()
                && std::env::var_os("SANDBOX_PREPARE_UI_PREVIEW").is_some()
            {
                match std::env::var("SANDBOX_PREPARE_UI_PREVIEW_MENU").as_deref() {
                    Ok("branch") => Some(0),
                    Ok("worktree") => Some(2),
                    _ => None,
                }
            } else {
                None
            },
        }
    }
    fn dismiss(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self
            .menu_bounds
            .get()
            .is_some_and(|b| b.contains(&position))
            && !self
                .trigger_bounds
                .get()
                .is_some_and(|b| b.contains(&position))
        {
            self.close(cx);
        }
    }
    fn finish(&mut self, cx: &mut Context<Self>) {
        self.close(cx);
        self.return_focus = true;
        cx.notify();
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.branches.update(cx, |p, cx| p.close(cx));
        self.worktrees.update(cx, |p, cx| p.close(cx));
        cx.notify();
    }
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.index = 0;
        self.focus.focus(window);
        cx.emit(SourceEvent::Opened);
        cx.notify();
    }
    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        self.branches.update(cx, |p, cx| p.set_theme(theme, cx));
        self.worktrees.update(cx, |p, cx| p.set_theme(theme, cx));
        cx.notify();
    }
    pub fn set(
        &mut self,
        label: String,
        mode: Option<SourceMode>,
        directory: String,
        status: String,
        available: bool,
        busy: bool,
        message: Option<String>,
        branches: Vec<ContextItem>,
        worktrees: Vec<ContextItem>,
        legacy: Vec<(SandboxId, String)>,
        cx: &mut Context<Self>,
    ) {
        self.label = label;
        self.mode = mode;
        self.directory = directory;
        self.status = status;
        self.available = available;
        self.busy = busy;
        self.message = message;
        self.legacy = legacy;
        self.branches.update(cx, |p, cx| {
            p.set_submenu_selected(
                matches!(self.mode, Some(SourceMode::ManagedWorktree { .. })),
                cx,
            );
            p.set(branches, String::new(), "Isolated Worktree".into(), cx);
            p.actions(
                vec![ContextAction {
                    id: "new-branch",
                    label: "Create branch from entered name",
                }],
                cx,
            );
        });
        self.worktrees.update(cx, |p, cx| {
            p.set_submenu_selected(
                matches!(self.mode, Some(SourceMode::ExistingWorktree { .. })),
                cx,
            );
            p.set(worktrees, String::new(), "Existing Git Worktree".into(), cx)
        });
        cx.notify();
    }
    fn source_count(&self) -> usize {
        if self.available && !self.busy { 3 } else { 0 }
    }
    fn action_count(&self) -> usize {
        self.source_count() + 1 + self.legacy.len()
    }
    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let sources = self.source_count();
        if index < sources {
            match index {
                0 => self.branches.update(cx, |p, cx| p.open(window, cx)),
                1 => {
                    self.finish(cx);
                    cx.emit(SourceEvent::Open(SourceMode::CurrentCheckout));
                }
                2 => self.worktrees.update(cx, |p, cx| p.open(window, cx)),
                _ => {}
            }
        } else if index == sources {
            if self.busy {
                cx.emit(SourceEvent::Cancel);
            } else {
                self.finish(cx);
                cx.emit(SourceEvent::Configure);
            }
        } else if let Some((id, _)) = self.legacy.get(index - sources - 1) {
            let id = id.clone();
            self.finish(cx);
            cx.emit(SourceEvent::Legacy(id));
        }
    }
    fn menu_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        let count = self.action_count();
        match event.keystroke.key.as_str() {
            "escape" => {
                self.close(cx);
                self.focus.focus(window);
            }
            "down" => self.index = (self.index + 1) % count,
            "up" => self.index = (self.index + count - 1) % count,
            "enter" | "right" => self.activate(self.index, window, cx),
            "tab" => {
                self.close(cx);
                return;
            }
            _ => return,
        }
        cx.notify();
        cx.stop_propagation();
    }
}
impl Render for SourceMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.return_focus) {
            cx.defer_in(window, |this, window, _| this.focus.focus(window));
        }
        if self.open
            && self.available
            && let Some(index) = self.preview_submenu.take()
        {
            cx.defer_in(window, move |this, window, cx| {
                this.activate(index, window, cx)
            });
        }
        let branch_popup = self
            .branches
            .update(cx, |p, cx| p.submenu_popup(window, cx));
        let worktree_popup = self
            .worktrees
            .update(cx, |p, cx| p.submenu_popup(window, cx));
        let p = self.theme.palette(window.appearance());
        let trigger_bounds = self.trigger_bounds.clone();
        let menu_bounds = self.menu_bounds.clone();
        let mut menu = div()
            .id("working-copy-menu")
            .occlude()
            .flex()
            .flex_col()
            .w(px(300.))
            .max_w(window.viewport_size().width - px(16.))
            .max_h((window.viewport_size().height - px(48.)).max(px(0.)))
            .overflow_y_scroll()
            .rounded_md()
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.panel_background))
            .shadow_md()
            .text_size(px(13.))
            .text_color(rgb(p.text))
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if !this.branches.read(cx).is_open() && !this.worktrees.read(cx).is_open() {
                    this.dismiss(event.position, cx);
                }
            }))
            .child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .text_size(px(12.))
                    .text_color(rgb(p.text_muted))
                    .child("Work In"),
            );
        if self.available && !self.busy {
            menu = menu.child(
                div()
                    .p(px(4.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .when(self.index == 0, |d| d.bg(rgb(p.element_hover)))
                            .child(self.branches.clone()),
                    )
                    .child(
                        div()
                            .id("current-checkout")
                            .h(px(30.))
                            .px(px(4.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .rounded_sm()
                            .cursor_pointer()
                            .when(self.index == 1, |d| d.bg(rgb(p.element_hover)))
                            .hover(|d| d.bg(rgb(p.element_hover)))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.activate(1, window, cx)),
                            )
                            .child(Icon::Worktree.render(p.icon_muted).size(px(12.)))
                            .child(div().flex_1().child("Existing Local Checkout"))
                            .when(
                                matches!(self.mode, Some(SourceMode::CurrentCheckout)),
                                |d| d.child(Icon::Check.render(p.icon_accent).size(px(12.))),
                            ),
                    )
                    .child(
                        div()
                            .when(self.index == 2, |d| d.bg(rgb(p.element_hover)))
                            .child(self.worktrees.clone()),
                    ),
            );
        }
        if let Some(message) = self.message.clone() {
            menu = menu.child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .text_color(rgb(p.text_muted))
                    .child(message),
            );
        }
        menu = menu.child(
            div()
                .border_t_1()
                .border_color(rgb(p.border))
                .px(px(12.))
                .py(px(8.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().truncate().child(self.label.clone()))
                .when(!self.directory.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.text_muted))
                            .truncate()
                            .child(self.directory.clone()),
                    )
                })
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(p.text_muted))
                        .child(self.status.clone()),
                ),
        );
        if self.busy {
            menu = menu.child(
                div()
                    .id("cancel-source")
                    .when(self.index == self.source_count(), |d| {
                        d.bg(rgb(p.element_hover))
                    })
                    .h(px(30.))
                    .px(px(12.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(p.element_hover)))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SourceEvent::Cancel)))
                    .child("Cancel preparation"),
            );
        } else {
            menu = menu.child(
                div()
                    .id("configure-source")
                    .when(self.index == self.source_count(), |d| {
                        d.bg(rgb(p.element_hover))
                    })
                    .h(px(30.))
                    .px(px(12.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(p.element_hover)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.finish(cx);
                        cx.emit(SourceEvent::Configure);
                    }))
                    .child("Configure launch…"),
            );
        }
        if !self.legacy.is_empty() {
            menu = menu.child(
                div()
                    .px(px(12.))
                    .py(px(4.))
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .text_size(px(12.))
                    .text_color(rgb(p.text_muted))
                    .child("Existing environments"),
            );
            for (i, (id, label)) in self.legacy.iter().enumerate() {
                let id = id.clone();
                menu = menu.child(
                    div()
                        .id(("legacy-environment", i))
                        .when(self.index == self.source_count() + 1 + i, |d| {
                            d.bg(rgb(p.element_hover))
                        })
                        .h(px(30.))
                        .px(px(12.))
                        .cursor_pointer()
                        .hover(|d| d.bg(rgb(p.element_hover)))
                        .child(label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.finish(cx);
                            cx.emit(SourceEvent::Legacy(id.clone()));
                        })),
                );
            }
        }
        let menu = menu.child(
            canvas(
                move |bounds, _, _| menu_bounds.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
        div()
            .on_key_down(cx.listener(Self::menu_key))
            .relative()
            .min_w_0()
            .font_family(style::FONT_FAMILY)
            .text_size(px(style::LABEL_SIZE))
            .text_color(rgb(p.text_muted))
            .child(
                div()
                    .id("working-copy-trigger")
                    .track_focus(&self.focus)
                    .h(px(style::BUTTON_HEIGHT))
                    .px(px(style::BUTTON_PADDING))
                    .flex()
                    .items_center()
                    .gap(px(style::ICON_GAP))
                    .min_w_0()
                    .rounded_sm()
                    .cursor_pointer()
                    .focus(|d| d.bg(rgb(p.element_hover)))
                    .hover(|d| d.bg(rgb(p.element_hover)))
                    .when(self.open, |d| {
                        d.bg(rgb(p.border_focused)).text_color(rgb(p.icon_accent))
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.open {
                            this.close(cx);
                        } else {
                            this.open(window, cx);
                        }
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if !this.open
                            && matches!(event.keystroke.key.as_str(), "enter" | "space" | "down")
                        {
                            this.open(window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        Icon::Worktree
                            .render(p.icon_muted)
                            .size(px(style::ICON_SIZE)),
                    )
                    .child(div().min_w_0().truncate().child(self.label.clone())),
            )
            .child(
                canvas(
                    move |bounds, _, _| trigger_bounds.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .when(self.open, |d| {
                d.child(
                    div().absolute().top_0().left_0().child(deferred(
                        anchored()
                            .offset(point(px(0.), px(style::BUTTON_HEIGHT + 4.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(menu),
                    )),
                )
            })
            .children(branch_popup)
            .children(worktree_popup)
    }
}
