//! The one single-line text entry used everywhere in the client.
//!
//! It supports the usual desktop editing set: character, word, and line
//! movement with or without selection, word deletion, mouse and double- or
//! triple-click selection, the clipboard, undo and redo, and input methods.
//! Enter emits [`Submitted`], so the owner can save without a second button.
use super::theme::{CONTROL, FAINT, FOCUS, INK, MONITOR, PANEL, RADIUS};
use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, EventEmitter,
    FocusHandle, Pixels, Point, ShapedLine, TextRun, UTF16Selection, Window, canvas, div, fill,
    point, prelude::*, px, rgb, rgba,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// The most undo steps a field keeps.
const HISTORY: usize = 100;

/// Emitted when the person presses Enter in a field.
pub(super) struct Submitted;

/// The kind of the last edit. Typing in a row joins one undo step.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Edit {
    #[default]
    None,
    Typing,
    Other,
}

#[derive(Clone, Default)]
struct Snapshot {
    value: String,
    anchor: usize,
    cursor: usize,
}

#[derive(Default)]
struct TextState {
    value: String,
    anchor: usize,
    cursor: usize,
    marked: Option<Range<usize>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Edit,
}

impl TextState {
    fn new(value: &str) -> Self {
        Self {
            value: value.to_owned(),
            anchor: value.len(),
            cursor: value.len(),
            ..Self::default()
        }
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    fn utf16(&self, range: Range<usize>) -> Range<usize> {
        self.value[..range.start].encode_utf16().count()
            ..self.value[..range.end].encode_utf16().count()
    }

    fn bytes(&self, range: Range<usize>) -> Range<usize> {
        let start = utf16_offset(&self.value, range.start);
        start..utf16_offset(&self.value, range.end).max(start)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { value: self.value.clone(), anchor: self.anchor, cursor: self.cursor }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.value = snapshot.value;
        self.anchor = snapshot.anchor.min(self.value.len());
        self.cursor = snapshot.cursor.min(self.value.len());
        self.marked = None;
        self.last_edit = Edit::None;
    }

    /// Saves the state before an edit. Consecutive typing is one step.
    fn record(&mut self, edit: Edit) {
        if !(edit == Edit::Typing && self.last_edit == Edit::Typing) {
            self.undo.push(self.snapshot());
            if self.undo.len() > HISTORY {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_edit = edit;
    }

    fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(previous);
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(next);
        }
    }

    fn replace(&mut self, range: Option<Range<usize>>, text: &str, marked: bool) -> Range<usize> {
        let range = range
            .map(|r| self.bytes(r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection());
        let text = text.replace(['\r', '\n', '\t'], " ");
        self.value.replace_range(range.clone(), &text);
        let inserted = range.start..range.start + text.len();
        self.anchor = inserted.end;
        self.cursor = inserted.end;
        self.marked = marked.then_some(inserted.clone());
        inserted
    }

    /// Deletes the selection, or the text between the cursor and `to`.
    fn delete_to(&mut self, to: usize) {
        if self.selection().is_empty() {
            self.anchor = to;
        }
        if !self.selection().is_empty() {
            self.record(Edit::Other);
            self.replace(None, "", false);
        }
    }

    fn move_cursor(&mut self, next: usize, extend: bool) {
        self.cursor = next;
        if !extend {
            self.anchor = next;
        }
        self.last_edit = Edit::None;
    }

    fn select(&mut self, range: Range<usize>) {
        self.anchor = range.start;
        self.cursor = range.end;
        self.last_edit = Edit::None;
    }

    fn previous(&self) -> usize {
        self.value[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.value[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    /// The start of the word before the cursor, skipping spaces.
    fn word_start(&self) -> usize {
        self.value[..self.cursor]
            .split_word_bound_indices()
            .rfind(|(_, word)| !word.trim().is_empty())
            .map_or(0, |(index, _)| index)
    }

    /// The end of the word after the cursor, skipping spaces.
    fn word_end(&self) -> usize {
        self.value[self.cursor..]
            .split_word_bound_indices()
            .find(|(_, word)| !word.trim().is_empty())
            .map_or(self.value.len(), |(index, word)| self.cursor + index + word.len())
    }

    /// The word, space run, or punctuation mark at a byte offset.
    fn word_at(&self, offset: usize) -> Range<usize> {
        self.value
            .split_word_bound_indices()
            .map(|(index, word)| index..index + word.len())
            .find(|range| range.contains(&offset))
            .unwrap_or(self.value.len()..self.value.len())
    }
}

fn utf16_offset(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= offset {
            return index;
        }
        units += ch.len_utf16();
    }
    text.len()
}

pub(super) struct TextField {
    pub(super) focus: FocusHandle,
    text: TextState,
    placeholder: &'static str,
    selecting: bool,
    layout: Option<(ShapedLine, Bounds<Pixels>, Pixels)>,
}

impl EventEmitter<Submitted> for TextField {}

impl TextField {
    pub(super) fn new(value: &str, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            text: TextState::new(value),
            placeholder: "",
            selecting: false,
            layout: None,
        }
    }

    /// Text shown in a faint color while the field is empty.
    pub(super) const fn with_placeholder(mut self, placeholder: &'static str) -> Self {
        self.placeholder = placeholder;
        self
    }

    pub(super) fn value(&self) -> &str {
        &self.text.value
    }

    /// Replaces the whole value as one undoable edit.
    pub(super) fn set_value(&mut self, value: &str, cx: &mut Context<Self>) {
        self.text.record(Edit::Other);
        self.text.value = value.replace(['\r', '\n', '\t'], " ");
        self.text.anchor = self.text.value.len();
        self.text.cursor = self.text.value.len();
        self.text.marked = None;
        cx.notify();
    }

    fn copy(&self, cx: &mut Context<Self>) {
        let selected = self.text.selection();
        if !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.text.value[selected].to_owned()));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.text.record(Edit::Other);
            self.text.replace(None, &text, false);
        }
    }

    fn key(&mut self, event: &gpui::KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let (control, shift) = (key.modifiers.control, key.modifiers.shift);
        let selected = self.text.selection();
        match (control, shift, key.key.as_str()) {
            (true, false, "a") => self.text.select(0..self.text.value.len()),
            (true, false, "c" | "insert") => self.copy(cx),
            (true, false, "x") | (false, true, "delete") => {
                self.copy(cx);
                self.text.delete_to(self.text.cursor);
            }
            (true, false, "v") | (false, true, "insert") => self.paste(cx),
            (true, false, "z") => self.text.undo(),
            (true, true, "z") | (true, false, "y") => self.text.redo(),
            (true, _, "left") => self.text.move_cursor(self.text.word_start(), shift),
            (true, _, "right") => self.text.move_cursor(self.text.word_end(), shift),
            (false, _, "left") => {
                let next = if !shift && !selected.is_empty() {
                    selected.start
                } else {
                    self.text.previous()
                };
                self.text.move_cursor(next, shift);
            }
            (false, _, "right") => {
                let next =
                    if !shift && !selected.is_empty() { selected.end } else { self.text.next() };
                self.text.move_cursor(next, shift);
            }
            (_, _, "home" | "up") => self.text.move_cursor(0, shift),
            (_, _, "end" | "down") => self.text.move_cursor(self.text.value.len(), shift),
            (true, _, "backspace") => self.text.delete_to(self.text.word_start()),
            (true, _, "delete") => self.text.delete_to(self.text.word_end()),
            (false, _, "backspace") => self.text.delete_to(self.text.previous()),
            (false, false, "delete") => self.text.delete_to(self.text.next()),
            (_, _, "enter") => {
                // A held key repeats. One press submits once.
                if !event.is_held {
                    cx.emit(Submitted);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn index_at(&self, position: Point<Pixels>) -> Option<usize> {
        let (line, bounds, offset) = self.layout.as_ref()?;
        Some(line.closest_index_for_x(position.x - bounds.left() + *offset))
    }

    fn mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        if let Some(index) = self.index_at(event.position) {
            match event.click_count {
                2 => self.text.select(self.text.word_at(index)),
                3.. => self.text.select(0..self.text.value.len()),
                _ => {
                    self.text.move_cursor(index, event.modifiers.shift);
                    self.selecting = true;
                }
            }
        }
        cx.notify();
    }

    fn mouse_move(&mut self, event: &gpui::MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting
            && event.dragging()
            && let Some(index) = self.index_at(event.position)
            && index != self.text.cursor
        {
            self.text.move_cursor(index, true);
            cx.notify();
        }
    }

    fn paint_text(
        input: &gpui::Entity<Self>,
        bounds: Bounds<Pixels>,
        (line, offset): (ShapedLine, Pixels),
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let field = input.read(cx);
        let focus = field.focus.clone();
        let selected = field.text.selection();
        if focus.is_focused(window) {
            let left = bounds.left() + line.x_for_index(selected.start) - offset;
            let right = bounds.left() + line.x_for_index(selected.end) - offset;
            window.paint_quad(fill(
                Bounds::from_corners(
                    point(left, bounds.top()),
                    point(if selected.is_empty() { left + px(1.) } else { right }, bounds.bottom()),
                ),
                if selected.is_empty() { rgb(MONITOR) } else { rgba(0x69d9_b340) },
            ));
        }
        window.handle_input(&focus, ElementInputHandler::new(bounds, input.clone()), cx);
        if field.text.value.is_empty() && !field.placeholder.is_empty() {
            let hint = shape(window, field.placeholder.to_owned(), FAINT);
            let _ = hint.paint(
                point(bounds.left(), bounds.top()),
                px(24.),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        let _ = line.paint(
            point(bounds.left() - offset, bounds.top()),
            px(24.),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
        input.update(cx, |field, _| field.layout = Some((line, bounds, offset)));
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = self.text.bytes(range);
        *actual = Some(self.text.utf16(bytes.clone()));
        Some(self.text.value[bytes].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.text.utf16(self.text.selection()),
            reversed: self.text.cursor < self.text.anchor,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.text.marked.clone().map(|r| self.text.utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.text.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let typing = self.text.marked.is_none()
            && self.text.selection().is_empty()
            && !text.trim().is_empty()
            && text.graphemes(true).count() == 1;
        self.text.record(if typing { Edit::Typing } else { Edit::Other });
        self.text.replace(range, text, false);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text.marked.is_none() {
            self.text.record(Edit::Other);
        }
        let inserted = self.text.replace(range, text, true);
        if let Some(selected) = selected {
            let value = &self.text.value[inserted.clone()];
            self.text.anchor = inserted.start + utf16_offset(value, selected.start);
            self.text.cursor = inserted.start + utf16_offset(value, selected.end);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (line, _, offset) = self.layout.as_ref()?;
        let range = self.text.bytes(range);
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(range.start) - *offset, bounds.top()),
            point(bounds.left() + line.x_for_index(range.end) - *offset, bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.index_at(point)?;
        Some(self.text.value[..index].encode_utf16().count())
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = cx.entity();
        let paint_input = input.clone();
        let focused = self.focus.is_focused(window);
        div()
            .id("text-entry")
            .track_focus(&self.focus)
            .tab_index(0)
            .role(gpui::Role::TextInput)
            .aria_value(self.text.value.clone())
            .when(!self.placeholder.is_empty(), |view| view.aria_placeholder(self.placeholder))
            .flex_1()
            .min_w(px(80.))
            .h(px(CONTROL))
            .px(px(10.))
            .py(px(3.))
            .bg(rgb(PANEL))
            .rounded(px(RADIUS))
            .border_1()
            .border_color(if focused { rgb(FOCUS) } else { rgba(0) })
            .overflow_hidden()
            .cursor_text()
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.selecting = false),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let field = input.read(cx);
                        let line = shape(window, field.text.value.clone(), INK);
                        let offset = (line.x_for_index(field.text.cursor) - bounds.size.width
                            + px(3.))
                        .max(px(0.));
                        (line, offset)
                    },
                    move |bounds, layout, window, cx| {
                        Self::paint_text(&paint_input, bounds, layout, window, cx);
                    },
                )
                .size_full(),
            )
    }
}

fn shape(window: &mut Window, text: String, color: u32) -> ShapedLine {
    let style = window.text_style();
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color: rgb(color).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window.text_system().shape_line(text.into(), px(14.), &[run], None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn keyboard_selection_deletes_whole_graphemes(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, cx| TextField::new("Ae\u{301}🎧", cx));
        cx.update(|window, cx| {
            let focus = view.read(cx).focus.clone();
            focus.focus(window, cx);
        });
        cx.simulate_keystrokes("end backspace");
        assert_eq!(view.read_with(cx, |view, _| view.value().to_owned()), "Ae\u{301}");
        cx.simulate_keystrokes("backspace");
        assert_eq!(view.read_with(cx, |view, _| view.value().to_owned()), "A");
        cx.simulate_keystrokes("ctrl-a backspace");
        assert_eq!(view.read_with(cx, |view, _| view.value().to_owned()), "");
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn word_shortcuts_clipboard_undo_and_submit_work_from_the_keyboard(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, cx| TextField::new("Hello brave world", cx));
        let submitted = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|window, cx| {
            let count = std::rc::Rc::clone(&submitted);
            cx.subscribe(&view, move |_, _: &Submitted, _| count.set(count.get() + 1)).detach();
            let focus = view.read(cx).focus.clone();
            focus.focus(window, cx);
        });
        let value =
            |cx: &mut gpui::VisualTestContext| view.read_with(cx, |v, _| v.value().to_owned());
        cx.simulate_keystrokes("ctrl-backspace");
        assert_eq!(value(cx), "Hello brave ");
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(value(cx), "Hello brave world");
        cx.simulate_keystrokes("ctrl-shift-z");
        assert_eq!(value(cx), "Hello brave ");
        cx.simulate_keystrokes("home ctrl-delete");
        assert_eq!(value(cx), " brave ");
        cx.simulate_keystrokes("ctrl-shift-right ctrl-x");
        assert_eq!(value(cx), " ");
        cx.simulate_keystrokes("end ctrl-v");
        assert_eq!(value(cx), "  brave");
        cx.simulate_keystrokes("enter");
        assert_eq!(submitted.get(), 1);
    }

    #[test]
    fn words_skip_spaces_and_typing_joins_one_undo_step() {
        // Unicode word boundaries keep a dotted ID such as `org.kde` together.
        let mut text = TextState::new("org.kde ark");
        assert_eq!(text.word_start(), 8);
        text.cursor = 0;
        assert_eq!(text.word_end(), 7);
        assert_eq!(text.word_at(5), 0..7);
        assert_eq!(text.word_at(7), 7..8);
        text.cursor = text.value.len();
        text.anchor = text.cursor;
        for character in ["s", "!"] {
            text.record(Edit::Typing);
            text.replace(None, character, false);
        }
        assert_eq!(text.value, "org.kde arks!");
        text.undo();
        assert_eq!(text.value, "org.kde ark");
        text.redo();
        assert_eq!(text.value, "org.kde arks!");
    }

    #[test]
    fn utf16_composition_and_grapheme_edits_keep_valid_text() {
        let mut text = TextState::new("A🎧e\u{301}");
        text.cursor = 0;
        text.anchor = 0;
        assert_eq!(text.bytes(1..3), 1..5);
        text.replace(Some(1..3), "音", true);
        assert_eq!(text.value, "A音e\u{301}");
        text.replace(None, "楽", false);
        assert_eq!(text.value, "A楽e\u{301}");
        text.cursor = text.value.len();
        text.anchor = text.previous();
        text.replace(None, "", false);
        assert_eq!(text.value, "A楽");
        assert_eq!(text.bytes(99..100), text.value.len()..text.value.len());
    }
}
