use crate::theme::ThemePalette as _;
// Text editing adapted from the GPUI 0.2.2 input example (Apache-2.0).
use std::ops::Range;

use crate::{icons::Icon, theme::ThemeMode};
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill,
    point, prelude::*, px, relative, rgb, size,
};
use unicode_segmentation::*;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Indent,
        Quit,
    ]
);

pub struct Edited;
impl gpui::EventEmitter<Edited> for TextInput {}

pub struct TextInput {
    secret: bool,
    multiline: bool,
    compact: bool,
    last_lines: Vec<(usize, ShapedLine)>,
    scroll: gpui::ScrollHandle,
    viewport_height: Pixels,
    theme: ThemeMode,
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl TextInput {
    pub fn new(value: String, secret: bool, theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        Self {
            secret,
            multiline: false,
            compact: false,
            last_lines: vec![],
            scroll: gpui::ScrollHandle::new(),
            viewport_height: px(200.),
            theme,
            focus_handle: cx.focus_handle().tab_stop(true),
            content: value.into(),
            placeholder: "".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }
    pub fn multiline(value: String, theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        let mut input = Self::new(value, false, theme, cx);
        input.multiline = true;
        input
    }
    pub fn set_value(&mut self, value: String, cx: &mut Context<Self>) {
        self.content = value.into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
    pub fn search(theme: ThemeMode, cx: &mut Context<Self>) -> Self {
        let mut input = Self::new(String::new(), false, theme, cx);
        input.placeholder = "Search settings…".into();
        input.compact = true;
        input
    }
    pub fn focus(&self, window: &mut Window) {
        self.focus_handle.focus(window);
    }
    fn reveal_cursor(&self) {
        if !self.multiline {
            return;
        }
        let y = px(self.content[..self.cursor_offset()]
            .bytes()
            .filter(|b| *b == b'\n')
            .count() as f32
            * 22.);
        let mut offset = self.scroll.offset();
        if y + offset.y < px(0.) {
            offset.y = -y;
        }
        if y + px(22.) + offset.y > self.viewport_height {
            offset.y = self.viewport_height - y - px(22.);
        }
        self.scroll.set_offset(offset);
    }
    fn editor_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.multiline {
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" => self.replace_text_in_range(None, "\n", window, cx),
            "up" | "down" => {
                let cursor = self.cursor_offset();
                let row = self.content[..cursor]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count();
                let next = if event.keystroke.key == "up" {
                    row.saturating_sub(1)
                } else {
                    (row + 1).min(self.last_lines.len().saturating_sub(1))
                };
                if let (Some((start, line)), Some((next_start, next_line))) =
                    (self.last_lines.get(row), self.last_lines.get(next))
                {
                    let offset = next_start
                        + next_line.closest_index_for_x(line.x_for_index(cursor - start));
                    if event.keystroke.modifiers.shift {
                        self.select_to(offset, cx);
                    } else {
                        self.move_to(offset, cx);
                    }
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }
    pub fn value(&self) -> String {
        self.content.to_string()
    }
    fn indent(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "  ", window, cx);
    }
    pub fn set_theme(&mut self, theme: ThemeMode, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window);
        self.is_selecting = true;

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text.replace("\r\n", "\n")
            } else {
                text.replace(['\r', '\n'], " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.secret && !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.secret && !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.reveal_cursor();
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.multiline {
            let Some(bounds) = self.last_bounds else {
                return 0;
            };
            let row = (f32::from(position.y - bounds.top()) / 22.).floor().max(0.) as usize;
            return self
                .last_lines
                .get(row.min(self.last_lines.len().saturating_sub(1)))
                .map(|(start, line)| start + line.closest_index_for_x(position.x - bounds.left()))
                .unwrap_or(0);
        }
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.reveal_cursor();
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.secret && !new_text.is_ascii() {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        let previous = self.content.clone();
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        if self.content != previous {
            cx.emit(Edited);
        }
        self.reveal_cursor();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.secret && !new_text.is_ascii() {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        let previous = self.content.clone();
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        if self.content != previous {
            cx.emit(Edited);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.multiline {
            let range = self.range_from_utf16(&range_utf16);
            let row = self.content[..range.start]
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
            let (start, line) = self.last_lines.get(row)?;
            let bounds = self.last_bounds?;
            let origin = point(
                bounds.left() + line.x_for_index(range.start - start),
                bounds.top() + px(row as f32 * 22.),
            );
            return Some(Bounds::new(origin, size(px(2.), px(22.))));
        }
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if self.multiline {
            return Some(self.offset_to_utf16(self.index_for_mouse_position(point)));
        }
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;

        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    lines: Vec<(usize, ShapedLine)>,
    selections: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.read(cx);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        if input.multiline {
            style.size.height = px(input.content.split('\n').count() as f32 * 22.).into();
            style.min_size.width = px(input
                .content
                .split('\n')
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0) as f32
                * 8.5)
            .into();
        }
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        if input.multiline {
            return multiline_prepaint(input, bounds, window);
        }
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (
                input.placeholder.clone(),
                rgb(input.theme.palette(window.appearance()).text_muted).into(),
            )
        } else {
            (
                if input.secret {
                    "*".repeat(content.len()).into()
                } else {
                    content
                },
                style.color,
            )
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = input.marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    rgb(input.theme.palette(window.appearance()).text_muted),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgb(input.theme.palette(window.appearance()).element_selected),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
            lines: vec![],
            selections: vec![],
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if self.input.read(cx).multiline {
            for selection in prepaint.selections.drain(..) {
                window.paint_quad(selection);
            }
            for (row, (_, line)) in prepaint.lines.iter().enumerate() {
                let _ = line.paint(
                    point(bounds.left(), bounds.top() + px(row as f32 * 22.)),
                    px(22.),
                    window,
                    cx,
                );
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
            let lines = std::mem::take(&mut prepaint.lines);
            self.input.update(cx, |input, _| {
                input.last_lines = lines;
                input.last_bounds = Some(bounds);
            });
            return;
        }
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let line = prepaint.line.take().unwrap();
        line.paint(bounds.origin, window.line_height(), window, cx)
            .unwrap();

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

fn multiline_prepaint(
    input: &TextInput,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) -> PrepaintState {
    let palette = input.theme.palette(window.appearance());
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let mut state = PrepaintState {
        line: None,
        cursor: None,
        selection: None,
        lines: vec![],
        selections: vec![],
    };
    let mut start = 0;
    let cursor = input.cursor_offset();
    for (row, text) in input.content.split('\n').enumerate() {
        let end = start + text.len();
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(text.to_owned().into(), font_size, &[run], None);
        let top = bounds.top() + px(row as f32 * 22.);
        if input.selected_range.is_empty() && (start..=end).contains(&cursor) {
            state.cursor = Some(fill(
                Bounds::new(
                    point(bounds.left() + line.x_for_index(cursor - start), top),
                    size(px(2.), px(22.)),
                ),
                rgb(palette.text_muted),
            ));
        } else if input.selected_range.start <= end && input.selected_range.end > start {
            let left = line.x_for_index(
                input
                    .selected_range
                    .start
                    .saturating_sub(start)
                    .min(text.len()),
            );
            let right = if input.selected_range.end > end {
                line.width + px(8.)
            } else {
                line.x_for_index(input.selected_range.end - start)
            };
            state.selections.push(fill(
                Bounds::new(
                    point(bounds.left() + left, top),
                    size((right - left).max(px(1.)), px(22.)),
                ),
                rgb(palette.element_selected),
            ));
        }
        state.lines.push((start, line));
        start = end + 1;
    }
    state
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.theme.palette(window.appearance());
        let background = if self.placeholder.is_empty() {
            palette.element_background
        } else {
            palette.editor_background
        };
        div()
            .flex()
            .key_context(if self.multiline {
                "TextInput JsonEditor"
            } else {
                "TextInput"
            })
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::editor_key))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::indent))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .bg(rgb(background))
            .text_color(rgb(palette.text))
            .rounded_sm()
            .overflow_hidden()
            .border_1()
            .border_color(rgb(palette.border))
            .focus(|style| style.border_color(rgb(palette.border_focused)))
            .line_height(px(if self.multiline {
                22.
            } else if self.compact {
                20.
            } else {
                30.
            }))
            .when(self.compact, |el| {
                el.h(px(28.)).items_center().px_2().gap_1()
            })
            .when(self.multiline, |el| {
                el.flex_col().size_full().min_h_0().font_family("Menlo")
            })
            .text_size(px(14.))
            .when(self.compact, |el| {
                el.child(Icon::Search.render(palette.icon_muted).size(px(14.)))
            })
            .child(
                div()
                    .id("input-content")
                    .h(px(if self.compact { 20. } else { 38. }))
                    .w_full()
                    .when(self.compact, |el| el.w(px(0.)).flex_1().min_w_0())
                    .when(self.multiline, |el| {
                        el.flex_1()
                            .h_full()
                            .min_h_0()
                            .overflow_scroll()
                            .track_scroll(&self.scroll)
                    })
                    .p(px(if self.compact { 0. } else { 4. }))
                    .bg(rgb(background))
                    .relative()
                    .child(TextElement { input: cx.entity() })
                    .when(self.multiline, |el| {
                        let input = cx.entity().downgrade();
                        el.child(
                            gpui::canvas(
                                move |bounds, _, cx| {
                                    let _ = input.update(cx, |input, _| {
                                        input.viewport_height = bounds.size.height
                                    });
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                    }),
            )
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", Indent, Some("JsonEditor")),
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("TextInput")),
        KeyBinding::new("cmd-v", Paste, Some("TextInput")),
        KeyBinding::new("cmd-c", Copy, Some("TextInput")),
        KeyBinding::new("cmd-x", Cut, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some("TextInput")),
    ]);
}
