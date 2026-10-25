//! Shared controls. Each one has visible hover, focus, and disabled states.
use super::Shell;
use super::theme::{
    BACKGROUND, BODY, BODY_LINE, CAPTION, CAPTION_LINE, CONTROL, FAINT, FOCUS, ICON_SMALL, INK,
    LABEL, LABEL_LINE, MUTED, PANEL, RADIUS, RADIUS_CARD, RAISED, RAISED_HOVER, SPACE_MD, SPACE_SM,
    alpha,
};
use gpui::{
    AnyView, App, Context, Div, FontFeatures, FontWeight, Render, SharedString, Stateful, Svg,
    Window, div, prelude::*, px, rgb, rgba, svg,
};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Tone {
    Primary,
    Secondary,
    Quiet,
}

pub(super) fn icon(name: &'static str, color: u32, extent: f32) -> Svg {
    svg().path(name).size(px(extent)).flex_shrink_0().text_color(rgb(color))
}

/// A small uppercase heading for a group of controls.
pub(super) fn label(text: &str) -> Div {
    div()
        .text_size(px(LABEL))
        .line_height(px(LABEL_LINE))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(FAINT))
        .child(text.to_uppercase())
}

pub(super) fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(CAPTION))
        .line_height(px(CAPTION_LINE))
        .text_color(rgb(MUTED))
        .child(text.into())
}

/// Fixed-width digits keep a changing level from moving its neighbors.
pub(super) fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)]))
}

/// A surface that is set apart by its fill, not by an outline.
pub(super) fn card() -> Div {
    div().rounded(px(RADIUS_CARD)).bg(rgb(PANEL))
}

pub(super) fn dot(color: u32) -> Div {
    div().size(px(8.)).flex_shrink_0().rounded_full().bg(rgb(color))
}

/// A square tile that carries one icon, tinted with the icon color.
pub(super) fn icon_tile(name: &'static str, color: u32, extent: f32) -> Div {
    div()
        .size(px(extent))
        .flex_shrink_0()
        .rounded(px(RADIUS))
        .bg(rgba(alpha(color, 0x1F)))
        .flex()
        .items_center()
        .justify_center()
        .child(icon(name, color, extent / 2.))
}

struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(280.))
            .mt(px(SPACE_SM))
            .px(px(SPACE_SM))
            .py(px(6.))
            .rounded(px(6.))
            .bg(rgb(RAISED_HOVER))
            .font_family("Geist")
            .text_size(px(CAPTION))
            .line_height(px(CAPTION_LINE))
            .text_color(rgb(INK))
            .child(self.0.clone())
    }
}

/// Names an icon-only control when the pointer rests on it.
pub(super) fn tip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip(text.clone())).into()
}

/// Adds pointer and keyboard activation to a focusable control.
pub(super) fn activate(
    control: Stateful<Div>,
    enabled: bool,
    cx: &mut Context<Shell>,
    action: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Stateful<Div> {
    let control = control.focusable().tab_index(0).tab_stop(enabled);
    if !enabled {
        return control.opacity(0.4);
    }
    let action = Rc::new(action);
    let click = Rc::clone(&action);
    control
        .cursor_pointer()
        .active(|style| style.opacity(0.8))
        .focus_visible(|style| style.border_color(rgb(FOCUS)))
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                cx.stop_propagation();
                action(this, window, cx);
            }
        }))
}

pub(super) fn identified(id: &str) -> Stateful<Div> {
    let selector = id.to_owned();
    div().id(id.to_owned()).debug_selector(move || selector)
}

/// The fill, ink, and hover fill of a button tone.
const fn tone_colors(tone: Tone) -> (Option<u32>, u32, u32) {
    match tone {
        Tone::Primary => (Some(INK), BACKGROUND, 0x00FF_FFFF),
        Tone::Secondary => (Some(RAISED), INK, RAISED_HOVER),
        Tone::Quiet => (None, MUTED, RAISED),
    }
}

/// An empty button surface. The transparent outline only shows keyboard focus.
pub(super) fn button_face(id: &str, tone: Tone) -> Stateful<Div> {
    let (fill, ink, hover) = tone_colors(tone);
    identified(id)
        .h(px(CONTROL))
        .px(px(SPACE_MD))
        .flex_shrink_0()
        .rounded(px(RADIUS))
        .border_1()
        .border_color(rgba(0))
        .when_some(fill, |view, fill| view.bg(rgb(fill)))
        .text_color(rgb(ink))
        .text_size(px(BODY))
        .line_height(px(BODY_LINE))
        .font_weight(FontWeight::MEDIUM)
        .flex()
        .items_center()
        .justify_center()
        .gap(px(SPACE_SM))
        .hover(move |style| style.bg(rgb(hover)))
}

pub(super) fn action_button(
    id: &str,
    label: &str,
    tone: Tone,
    enabled: bool,
    cx: &mut Context<Shell>,
    action: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Stateful<Div> {
    activate(button_face(id, tone).child(label.to_owned()), enabled, cx, action)
}

/// A button with a leading icon.
pub(super) fn labeled_icon_button(
    id: &str,
    glyph: &'static str,
    label: &str,
    tone: Tone,
    enabled: bool,
    cx: &mut Context<Shell>,
    action: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Stateful<Div> {
    let ink = tone_colors(tone).1;
    activate(
        button_face(id, tone).child(icon(glyph, ink, ICON_SMALL)).child(label.to_owned()),
        enabled,
        cx,
        action,
    )
}

/// A square button that shows only an icon. `hint` names it on hover.
#[allow(clippy::too_many_arguments)]
pub(super) fn icon_button(
    id: &str,
    glyph: &'static str,
    hint: &'static str,
    tone: Tone,
    ink: u32,
    enabled: bool,
    cx: &mut Context<Shell>,
    action: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Stateful<Div> {
    activate(
        button_face(id, tone)
            .w(px(CONTROL))
            .p_0()
            .child(icon(glyph, ink, ICON_SMALL))
            .tooltip(tip(hint)),
        enabled,
        cx,
        action,
    )
}
