//! The window frame: title bar, device stage, and the mixer area.
use super::controls::{
    Tone, action_button, caption, dot, icon, icon_button, identified, labeled_icon_button,
};
use super::mixer::{MIXER_MIN_HEIGHT, mixer_min_width};
use super::stage::{INSET, STAGE_WIDTH};
use super::theme::{
    BACKGROUND, BODY, BODY_LINE, DANGER, FAINT, HEADING, HEADING_LINE, ICON, INK, MONITOR, MUTED,
    RADIUS, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, TITLE, TITLE_LINE, WARNING, alpha,
};
use super::{Shell, TITLEBAR};
use gpui::{Context, Div, FontWeight, IntoElement, div, prelude::*, px, rgb, rgba};
use librewave_core::MixerRuntimeState;

impl Shell {
    pub(super) fn frame(&self, viewport: gpui::Size<gpui::Pixels>, cx: &mut Context<Self>) -> Div {
        // Below this width the stage moves above the mixer.
        let compact = f32::from(viewport.width) < STAGE_WIDTH + mixer_min_width() + 2. * INSET;
        let height = f32::from(viewport.height);
        let stage = self.stage(compact, height, cx);
        let body = if compact {
            div()
                .id("canvas")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(div().flex().flex_col().child(stage).child(self.work_area(true, cx)))
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
                        .child(self.work_area(false, cx)),
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
            .capture_key_down(cx.listener(Self::frame_key))
            // A drag ends wherever the pointer is released.
            .capture_any_mouse_up(cx.listener(|this, _, _, cx| this.release_preview(cx)))
            .child(self.titlebar(cx))
            .child(body)
            .when(self.application_input.is_some(), |view| view.child(channel_backdrop(cx)))
            .when_some(self.settings_modal.as_ref(), |view, modal| {
                view.child(self.settings_overlay(modal, cx))
            })
            .when_some(self.confirmation.as_ref(), |view, confirmation| {
                view.child(self.confirmation_overlay(confirmation, cx))
            })
    }

    /// Window keys: dialogs first, then Settings, then the main view.
    fn frame_key(
        this: &mut Self,
        event: &gpui::KeyDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(confirmation) = &this.confirmation {
            match event.keystroke.key.as_str() {
                "escape" => this.cancel_confirmation(cx),
                "enter" if confirmation.focus.is_focused(window) => {
                    this.accept_confirmation(cx);
                }
                "tab" => {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    if let Some(confirmation) = &this.confirmation
                        && !confirmation.focus.contains_focused(window, cx)
                    {
                        confirmation.focus.focus(window, cx);
                    }
                }
                // Enter and space reach the focused dialog button.
                "enter" | "space" => return,
                _ => {}
            }
            cx.stop_propagation();
        } else if this.settings_modal.is_some() {
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
        } else if event.keystroke.key == "escape" && this.application_input.is_some() {
            this.close_channel(cx);
            cx.stop_propagation();
        } else if event.keystroke.key == "tab" {
            if event.keystroke.modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            cx.stop_propagation();
        }
    }

    /// The mixer, or one notice when the daemon is unavailable.
    fn work_area(&self, compact: bool, cx: &mut Context<Self>) -> Div {
        let mut area = div()
            .w_full()
            .when(!compact, |view| view.h_full().min_h(px(MIXER_MIN_HEIGHT + 2. * INSET)))
            .p(px(INSET))
            .when(compact, gpui::Styled::pt_0)
            .flex()
            .flex_col()
            .gap(px(SPACE_XL));
        if self.settings_modal.is_none()
            && let Some(error) = self.action_error.clone()
        {
            area = area.child(error_banner(error, true, cx));
        }
        match &self.snapshot {
            Some(snapshot) => area.child(self.mixer_section(snapshot, compact, cx)),
            None => area.child(self.offline_notice(cx)),
        }
    }

    fn offline_notice(&self, cx: &mut Context<Self>) -> Div {
        let enabled = !self.busy() && self.settings_modal.is_none();
        div()
            .flex_1()
            .flex()
            .justify_center()
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
                        "Start mixer",
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
            .pl(px(INSET))
            .pr(px(SPACE_LG))
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
        let enabled = !self.busy() && self.settings_modal.is_none();
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
        match (
            self.lifecycle_pending,
            self.snapshot.as_ref().map(|snapshot| snapshot.mixer.runtime),
        ) {
            (Some(label), _) => control.child(state(WARNING, label)),
            (None, Some(MixerRuntimeState::Active)) => {
                control.child(state(MONITOR, "Mixer ready")).child(labeled_icon_button(
                    "titlebar-stop",
                    "stop",
                    "Stop",
                    Tone::Quiet,
                    enabled,
                    cx,
                    |this, _, cx| this.run_lifecycle(librewavectl::SessionAction::Stop, cx),
                ))
            }
            (None, runtime) => control
                .child(state(
                    FAINT,
                    if runtime.is_some() { "Daemon connected" } else { "Daemon disconnected" },
                ))
                .child(labeled_icon_button(
                    "titlebar-start",
                    "play",
                    "Start mixer",
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

/// A click outside the channel pop-up closes it and does nothing else.
fn channel_backdrop(cx: &mut Context<Shell>) -> impl IntoElement {
    div()
        .id("channel-menu-backdrop")
        .absolute()
        .inset_0()
        .occlude()
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.close_channel(cx);
                cx.stop_propagation();
            }),
        )
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}
