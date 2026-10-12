//! The window frame: title bar, device stage, and the mixer area.
use super::controls::{
    Tone, action_button, caption, dot, icon, icon_button, identified, labeled_icon_button,
};
use super::mixer::{mixer_min_width, mixer_width};
use super::stage::STAGE_WIDTH;
use super::theme::{
    BACKGROUND, BODY, BODY_LINE, DANGER, FAINT, HEADING, HEADING_LINE, ICON, INK, MONITOR, MUTED,
    RADIUS, SPACE_2XL, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, TITLE, TITLE_LINE,
    WARNING, alpha,
};
use super::{Shell, TITLEBAR};
use gpui::{Context, Div, FontWeight, IntoElement, div, prelude::*, px, rgb, rgba};
use librewave_core::MixerRuntimeState;

impl Shell {
    pub(super) fn frame(&self, viewport: gpui::Size<gpui::Pixels>, cx: &mut Context<Self>) -> Div {
        let inputs =
            self.snapshot.as_ref().map_or(0, |snapshot| snapshot.mixer.profile.sources.len());
        // Below this width the stage moves above the mixer.
        let compact = f32::from(viewport.width) < STAGE_WIDTH + mixer_min_width() + 2. * SPACE_2XL;
        let height = f32::from(viewport.height);
        let stage = self.stage(compact, height, cx);
        let body = if compact {
            div()
                .id("canvas")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(div().flex().flex_col().child(stage).child(self.work_area(
                    inputs,
                    f32::INFINITY,
                    cx,
                )))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(stage)
                .child(
                    div()
                        .id("canvas")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .child(self.work_area(inputs, height, cx)),
                )
                .into_any_element()
        };
        div()
            .size_full()
            .relative()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(INK))
            .font_family("Geist")
            .text_size(px(BODY))
            .line_height(px(BODY_LINE))
            .flex()
            .flex_col()
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if this.settings_modal.is_some() {
                    match event.keystroke.key.as_str() {
                        "escape" => this.close_settings(window, cx),
                        "tab" => {
                            if event.keystroke.modifiers.shift {
                                window.focus_prev(cx);
                            } else {
                                window.focus_next(cx);
                            }
                            if let Some(modal) = &this.settings_modal
                                && !modal.focus.contains_focused(window, cx)
                            {
                                modal.close_focus.focus(window, cx);
                            }
                        }
                        _ => {
                            if let Some(modal) = &this.settings_modal
                                && !modal.focus.contains_focused(window, cx)
                            {
                                modal.close_focus.focus(window, cx);
                                cx.stop_propagation();
                            }
                            return;
                        }
                    }
                    cx.stop_propagation();
                } else if event.keystroke.key == "," && event.keystroke.modifiers.control {
                    this.open_settings(window, cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .child(self.titlebar(cx))
            .child(body)
            .when_some(self.settings_modal.as_ref(), |view, modal| {
                view.child(self.settings_overlay(modal, cx))
            })
    }

    /// The mixer, or one notice when the daemon is unavailable.
    fn work_area(&self, inputs: usize, viewport_height: f32, cx: &mut Context<Self>) -> Div {
        let mut column =
            div().w_full().max_w(px(mixer_width(inputs))).flex().flex_col().gap(px(SPACE_XL));
        if self.settings_modal.is_none()
            && let Some(error) = self.action_error.clone()
        {
            column = column.child(error_banner(error, true, cx));
        }
        let column = match &self.snapshot {
            Some(snapshot) => column.child(self.mixer_section(snapshot, viewport_height, cx)),
            None => column.child(self.offline_notice(cx)),
        };
        div().w_full().p(px(SPACE_2XL)).flex().flex_col().items_center().child(column)
    }

    fn offline_notice(&self, cx: &mut Context<Self>) -> Div {
        let enabled = !self.pending && self.settings_modal.is_none();
        div()
            .py(px(96.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(SPACE_LG))
            .child(icon("plugs", FAINT, 40.))
            .child(
                div()
                    .text_size(px(TITLE))
                    .line_height(px(TITLE_LINE))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Not connected to LibreWave"),
            )
            .child(
                div()
                    .pt(px(SPACE_SM))
                    .flex()
                    .gap(px(SPACE_SM))
                    .child(labeled_icon_button(
                        "offline-start",
                        "play",
                        "Start audio",
                        Tone::Primary,
                        enabled,
                        cx,
                        Shell::start_audio,
                    ))
                    .child(action_button(
                        "offline-settings",
                        "Open Settings",
                        Tone::Secondary,
                        self.settings_modal.is_none(),
                        cx,
                        Shell::open_settings,
                    )),
            )
    }

    fn titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut mark = div().h(px(20.)).flex().items_center().gap(px(2.));
        for height in [8., 14., 18., 14., 8.] {
            mark = mark.child(div().w(px(2.5)).h(px(height)).rounded_full().bg(rgb(INK)));
        }
        let enabled = self.settings_modal.is_none();
        let mut settings = icon_button(
            "settings",
            "settings",
            "Settings",
            Tone::Quiet,
            MUTED,
            enabled,
            cx,
            Shell::open_settings,
        );
        if let Some(focus) = &self.settings_trigger_focus {
            settings = settings.track_focus(focus).tab_stop(enabled);
        }
        div()
            .h(px(TITLEBAR))
            .flex_shrink_0()
            .pl(px(SPACE_XL))
            .pr(px(SPACE_MD))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div().flex().items_center().gap(px(SPACE_MD)).child(mark).child(
                    div()
                        .text_size(px(HEADING))
                        .line_height(px(HEADING_LINE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("LibreWave"),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_SM))
                    .child(self.audio_control(cx))
                    .child(settings),
            )
    }

    /// The one place on the main view that states and changes the audio state.
    fn audio_control(&self, cx: &mut Context<Self>) -> Div {
        let enabled = !self.pending && self.settings_modal.is_none();
        let state = |color, text: &'static str| {
            div()
                .h_full()
                .px(px(SPACE_SM))
                .flex()
                .items_center()
                .gap(px(SPACE_SM))
                .child(dot(color))
                .child(caption(text))
        };
        let control = div().h(px(32.)).flex().items_center().gap(px(SPACE_XS));
        match (self.pending, self.snapshot.as_ref().map(|snapshot| snapshot.mixer.runtime)) {
            (true, _) => control.child(state(WARNING, "Working…")),
            (false, Some(MixerRuntimeState::Active)) => {
                control.child(state(MONITOR, "Audio running")).child(labeled_icon_button(
                    "titlebar-stop",
                    "stop",
                    "Stop",
                    Tone::Quiet,
                    enabled,
                    cx,
                    |this, _, cx| this.run_lifecycle(librewavectl::SessionAction::Stop, cx),
                ))
            }
            (false, _) => control.child(labeled_icon_button(
                "titlebar-start",
                "play",
                "Start audio",
                Tone::Primary,
                enabled,
                cx,
                Shell::start_audio,
            )),
        }
    }

    pub(super) fn start_audio(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) {
        self.run_lifecycle(
            librewavectl::SessionAction::Start { suspend_config: self.suspend_config.clone() },
            cx,
        );
    }
}

/// An error with its icon. A dismissible banner clears the action error.
pub(super) fn error_banner(error: String, dismissible: bool, cx: &mut Context<Shell>) -> Div {
    div()
        .w_full()
        .p(px(SPACE_MD))
        .rounded(px(RADIUS))
        .bg(rgba(alpha(DANGER, 0x1F)))
        .flex()
        .items_center()
        .gap(px(SPACE_MD))
        .child(icon("warning-circle", DANGER, ICON))
        .child(identified("action-error").flex_1().min_w_0().child(error))
        .when(dismissible, |view| {
            view.child(icon_button(
                "dismiss-error",
                "x",
                "Dismiss",
                Tone::Quiet,
                MUTED,
                true,
                cx,
                |this, _, cx| {
                    this.action_error = None;
                    cx.notify();
                },
            ))
        })
}
