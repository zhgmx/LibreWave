use super::theme::{BODY, BODY_LINE, CONTROL, RADIUS, SPACE_MD};
use super::{BACKGROUND, BORDER, INK, MUTED, RAISED, Shell};
use gpui::{Context, Div, FontWeight, Stateful, Window, div, prelude::*, px, rgb};
use std::rc::Rc;

#[derive(Clone, Copy)]
pub(super) enum Tone {
    Primary,
    Secondary,
    Quiet,
    Selected,
}

pub(super) fn button_face(id: &str, label: &str, tone: Tone, enabled: bool) -> Stateful<Div> {
    let (background, ink, border) = match tone {
        Tone::Primary => (INK, BACKGROUND, INK),
        Tone::Secondary => (RAISED, INK, BORDER),
        Tone::Quiet => (BACKGROUND, MUTED, BACKGROUND),
        Tone::Selected => (BORDER, INK, MUTED),
    };
    div()
        .id(id.to_owned())
        .debug_selector({
            let id = id.to_owned();
            move || id
        })
        .h(px(CONTROL))
        .px(px(SPACE_MD))
        .rounded(px(RADIUS))
        .border_1()
        .border_color(rgb(border))
        .bg(rgb(background))
        .text_color(rgb(ink))
        .text_size(px(BODY))
        .line_height(px(BODY_LINE))
        .font_weight(FontWeight::MEDIUM)
        .flex()
        .items_center()
        .justify_center()
        .focusable()
        .tab_index(0)
        .tab_stop(enabled)
        .when(enabled, |view| {
            view.cursor_pointer()
                .hover(|style| style.border_color(rgb(MUTED)))
                .active(|style| style.opacity(0.8))
                .focus_visible(|style| style.border_color(rgb(0x0070_B7FF)))
        })
        .when(!enabled, |view| view.opacity(0.4))
        .child(label.to_owned())
}

pub(super) fn action_button(
    id: &str,
    label: &str,
    tone: Tone,
    enabled: bool,
    cx: &mut Context<Shell>,
    action: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Stateful<Div> {
    let mut button = button_face(id, label, tone, enabled);
    if enabled {
        let action = Rc::new(action);
        let click = Rc::clone(&action);
        button = button
            .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    action(this, window, cx);
                }
            }));
    }
    button
}
