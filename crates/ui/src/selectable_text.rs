//! Read-only, wrapping text with native GPUI shaping and byte-accurate selection.
use crate::theme::Palette;
use gpui::{
    App, ClipboardItem, ElementId, FocusHandle, HighlightStyle, IntoElement, KeyBinding,
    MouseButton, RenderOnce, SharedString, StyledText, TextLayout, Window, actions, div,
    prelude::*, rgb,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

actions!(selectable_text, [Copy, SelectAll, Clear]);
#[derive(Default)]
struct Selection {
    anchor: usize,
    range: Range<usize>,
    dragging: bool,
}
impl Selection {
    fn extend(&mut self, end: usize) {
        self.range = self.anchor.min(end)..self.anchor.max(end);
    }
}
struct State {
    text: SharedString,
    selection: Selection,
    layout: TextLayout,
    focus: FocusHandle,
}
#[derive(IntoElement)]
pub struct SelectableText {
    id: ElementId,
    text: SharedString,
    palette: Palette,
}
impl SelectableText {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>, palette: Palette) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            palette,
        }
    }
}
fn word(text: &str, index: usize) -> Range<usize> {
    text.split_word_bound_indices()
        .find(|(start, value)| *start <= index && index < *start + value.len())
        .map(|(start, value)| start..start + value.len())
        .unwrap_or(index..index)
}
impl RenderOnce for SelectableText {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(
            SharedString::from(format!("text-selection:{:?}", self.id)),
            cx,
            |_, cx| State {
                text: self.text.clone(),
                selection: Selection::default(),
                layout: TextLayout::default(),
                focus: cx.focus_handle(),
            },
        );
        let (range, focus) = state.update(cx, |s, _| {
            if s.text != self.text {
                s.text = self.text.clone();
                s.selection = Selection::default();
            }
            (s.selection.range.clone(), s.focus.clone())
        });
        let text = StyledText::new(self.text).with_highlights((!range.is_empty()).then_some((
            range,
            HighlightStyle {
                background_color: Some(rgb(self.palette.element_selected).into()),
                ..Default::default()
            },
        )));
        state.update(cx, |s, _| s.layout = text.layout().clone());
        let down = state.clone();
        let movement = state.clone();
        let up = state.clone();
        let out = state.clone();
        let copy = state.clone();
        let all = state.clone();
        div()
            .id(self.id)
            .min_w_0()
            .cursor_text()
            .key_context("SelectableText")
            .track_focus(&focus)
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                down.update(cx, |s, cx| {
                    s.focus.focus(window);
                    let index = s
                        .layout
                        .index_for_position(event.position)
                        .unwrap_or_else(|index| index)
                        .min(s.text.len());
                    if event.click_count >= 3 {
                        s.selection.anchor = 0;
                        s.selection.range = 0..s.text.len();
                    } else if event.click_count == 2 {
                        s.selection.range = word(&s.text, index);
                        s.selection.anchor = s.selection.range.start;
                    } else if event.modifiers.shift {
                        s.selection.extend(index);
                    } else {
                        s.selection.anchor = index;
                        s.selection.range = index..index;
                    }
                    s.selection.dragging = true;
                    cx.notify();
                });
                cx.stop_propagation();
            })
            .on_mouse_move(move |event, _, cx| {
                movement.update(cx, |s, cx| {
                    if s.selection.dragging && event.dragging() {
                        let end = s
                            .layout
                            .index_for_position(event.position)
                            .unwrap_or_else(|index| index)
                            .min(s.text.len());
                        s.selection.extend(end);
                        cx.notify();
                    }
                });
            })
            .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                up.update(cx, |s, _| s.selection.dragging = false);
            })
            .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                out.update(cx, |s, _| s.selection.dragging = false);
            })
            .on_action(move |_: &Copy, _, cx| {
                copy.update(cx, |s, cx| {
                    if let Some(text) = s
                        .text
                        .get(s.selection.range.clone())
                        .filter(|text| !text.is_empty())
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                    }
                });
                cx.stop_propagation();
            })
            .on_action(move |_: &SelectAll, _, cx| {
                all.update(cx, |s, cx| {
                    s.selection.anchor = 0;
                    s.selection.range = 0..s.text.len();
                    cx.notify();
                });
                cx.stop_propagation();
            })
            .on_action(move |_: &Clear, _, cx| {
                state.update(cx, |s, cx| {
                    s.selection = Selection::default();
                    cx.notify();
                });
                cx.stop_propagation();
            })
            .child(text)
    }
}
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some("SelectableText")),
        KeyBinding::new("ctrl-c", Copy, Some("SelectableText")),
        KeyBinding::new("cmd-a", SelectAll, Some("SelectableText")),
        KeyBinding::new("ctrl-a", SelectAll, Some("SelectableText")),
        KeyBinding::new("escape", Clear, Some("SelectableText")),
    ]);
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_selection_uses_valid_byte_ranges_in_both_directions() {
        let text = "Ошибка базы: 日本語";
        assert_eq!(&text[word(text, 0)], "Ошибка");
        let mut selection = Selection {
            anchor: "Ошибка".len(),
            ..Default::default()
        };
        selection.extend(0);
        assert_eq!(&text[selection.range], "Ошибка");
    }
}
