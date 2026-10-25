//! Single-line native text entry for channel names and application identities.
use super::theme::{FAINT, INK, MONITOR, PANEL};
use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle, Pixels,
    Point, ShapedLine, TextRun, UTF16Selection, Window, canvas, div, fill, point, prelude::*, px,
    rgb, rgba,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
struct TextState {
    value: String,
    anchor: usize,
    cursor: usize,
    marked: Option<Range<usize>>,
}
impl TextState {
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
    fn move_cursor(&mut self, next: usize, extend: bool) {
        self.cursor = next;
        if !extend {
            self.anchor = next;
        }
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
    layout: Option<(ShapedLine, Bounds<Pixels>, Pixels)>,
}
impl TextField {
    pub(super) fn new(value: &str, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            text: TextState {
                value: value.to_owned(),
                anchor: 0,
                cursor: value.len(),
                marked: None,
            },
            placeholder: "",
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
    fn key(&mut self, event: &gpui::KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let selected = self.text.selection();
        match (key.modifiers.control, key.key.as_str()) {
            (true, "a") => {
                self.text.anchor = 0;
                self.text.cursor = self.text.value.len();
            }
            (true, "c" | "x") => {
                if !selected.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        self.text.value[selected].to_owned(),
                    ));
                    if key.key == "x" {
                        self.text.replace(None, "", false);
                    }
                }
            }
            (true, "v") => {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    self.text.replace(None, &text, false);
                }
            }
            (false, "left") => {
                let next = if !key.modifiers.shift && !selected.is_empty() {
                    selected.start
                } else {
                    self.text.previous()
                };
                self.text.move_cursor(next, key.modifiers.shift);
            }
            (false, "right") => {
                let next = if !key.modifiers.shift && !selected.is_empty() {
                    selected.end
                } else {
                    self.text.next()
                };
                self.text.move_cursor(next, key.modifiers.shift);
            }
            (_, "home") => self.text.move_cursor(0, key.modifiers.shift),
            (_, "end") => self.text.move_cursor(self.text.value.len(), key.modifiers.shift),
            (false, "backspace") => {
                if selected.is_empty() {
                    self.text.anchor = self.text.previous();
                }
                self.text.replace(None, "", false);
            }
            (false, "delete") => {
                if selected.is_empty() {
                    self.text.anchor = self.text.next();
                }
                self.text.replace(None, "", false);
            }
            (_, "enter") => {}
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
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
        let (line, bounds, offset) = self.layout.as_ref()?;
        let index = line.closest_index_for_x(point.x - bounds.left() + *offset);
        Some(self.text.value[..index].encode_utf16().count())
    }
}
impl Render for TextField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = cx.entity();
        let paint_input = input.clone();
        div()
            .id("text-entry")
            .track_focus(&self.focus)
            .tab_index(0)
            .flex_1()
            .min_w(px(80.))
            .h(px(36.))
            .px(px(8.))
            .py(px(6.))
            .bg(rgb(PANEL))
            .rounded(px(4.))
            .overflow_hidden()
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.focus.focus(window, cx);
                    if let Some((line, bounds, offset)) = &this.layout {
                        let index =
                            line.closest_index_for_x(event.position.x - bounds.left() + *offset);
                        this.text.move_cursor(index, event.modifiers.shift);
                    }
                    cx.notify();
                }),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let field = input.read(cx);
                        let style = window.text_style();
                        let run = TextRun {
                            len: field.text.value.len(),
                            font: style.font(),
                            color: rgb(INK).into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let line = window.text_system().shape_line(
                            field.text.value.clone().into(),
                            px(14.),
                            &[run],
                            None,
                        );
                        let offset = (line.x_for_index(field.text.cursor) - bounds.size.width
                            + px(3.))
                        .max(px(0.));
                        (line, offset)
                    },
                    move |bounds, (line, offset), window, cx| {
                        let field = paint_input.read(cx);
                        let focus = field.focus.clone();
                        let selected = field.text.selection();
                        if focus.is_focused(window) {
                            let left = bounds.left() + line.x_for_index(selected.start) - offset;
                            let right = bounds.left() + line.x_for_index(selected.end) - offset;
                            window.paint_quad(fill(
                                Bounds::from_corners(
                                    point(left, bounds.top()),
                                    point(
                                        if selected.is_empty() { left + px(1.) } else { right },
                                        bounds.bottom(),
                                    ),
                                ),
                                if selected.is_empty() { rgb(MONITOR) } else { rgba(0x69d9_b340) },
                            ));
                        }
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, paint_input.clone()),
                            cx,
                        );
                        if field.text.value.is_empty() {
                            paint_placeholder(field.placeholder, bounds, window, cx);
                        }
                        let _ = line.paint(
                            point(bounds.left() - offset, bounds.top()),
                            px(24.),
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                        paint_input
                            .update(cx, |field, _| field.layout = Some((line, bounds, offset)));
                    },
                )
                .size_full(),
            )
    }
}

/// Faint text for an empty field.
fn paint_placeholder(
    text: &'static str,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    if text.is_empty() {
        return;
    }
    let style = window.text_style();
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color: rgb(FAINT).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let hint = window.text_system().shape_line(text.into(), px(14.), &[run], None);
    let _ = hint.paint(
        point(bounds.left(), bounds.top()),
        px(24.),
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
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

    #[test]
    fn utf16_composition_and_grapheme_edits_keep_valid_text() {
        let mut text =
            TextState { value: "A🎧e\u{301}".into(), anchor: 0, cursor: 0, marked: None };
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
