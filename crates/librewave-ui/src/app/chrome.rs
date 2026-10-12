use super::{
    BACKGROUND, BORDER, INK, MUTED, SettingsModal, Shell, WARNING, action_button,
    controls::Tone,
    theme::{
        BODY, BODY_LINE, CAPTION, CAPTION_LINE, CONTROL, HEADING, HEADING_LINE, ICON, RADIUS,
        SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL,
    },
};
use gpui::{Context, FontWeight, IntoElement, Svg, div, prelude::*, px, rgb, rgba, svg};
use librewave_core::{Command, MixerRuntimeState};

pub(super) fn icon(name: &'static str, color: u32, extent: f32) -> Svg {
    svg().path(name).size(px(extent)).text_color(rgb(color))
}

impl Shell {
    pub(super) fn mixer_frame(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(
                div()
                    .id("mixer-canvas")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.mixer_channels(cx))
                    .child(self.output_rows()),
            )
            .when_some(self.settings_modal.as_ref(), |view, modal| {
                view.child(self.settings_overlay(modal, cx))
            })
    }

    fn titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = match (
            self.pending,
            self.action_error.is_some(),
            self.snapshot.as_ref().map(|s| s.mixer.runtime),
        ) {
            (true, _, _) => "Working…",
            (_, true, _) => "Needs attention",
            (_, _, None) => "Not connected",
            (_, _, Some(MixerRuntimeState::Inactive { .. })) => "Audio stopped",
            (_, _, Some(MixerRuntimeState::Active)) => "Audio running",
        };
        let mut mark = div().h(px(SPACE_XL)).flex().items_center().gap(px(3.));
        for height in [12., 20., 24., 20., 12.] {
            mark = mark.child(div().w(px(3.)).h(px(height)).rounded(px(2.)).bg(rgb(INK)));
        }
        let enabled = self.settings_modal.is_none();
        let mut settings =
            action_button("settings", "Settings", Tone::Quiet, enabled, cx, |this, window, cx| {
                this.open_settings(window, cx);
            })
            .gap(px(SPACE_SM))
            .child(icon("settings", INK, ICON));
        if let Some(focus) = &self.settings_trigger_focus {
            settings = settings.track_focus(focus).tab_stop(enabled);
        }
        div()
            .h(px(64.))
            .flex_shrink_0()
            .px(px(SPACE_XL))
            .border_b_1()
            .border_color(rgb(BORDER))
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
                    .gap(px(SPACE_LG))
                    .child(
                        div()
                            .text_size(px(CAPTION))
                            .line_height(px(CAPTION_LINE))
                            .text_color(rgb(if self.action_error.is_some() {
                                WARNING
                            } else {
                                MUTED
                            }))
                            .child(state),
                    )
                    .child(settings),
            )
    }

    fn output_rows(&self) -> impl IntoElement {
        let active = self
            .snapshot
            .as_ref()
            .is_some_and(|s| matches!(s.mixer.runtime, MixerRuntimeState::Active));
        let mut outputs = div()
            .p(px(SPACE_XL))
            .border_t_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap(px(SPACE_LG))
            .child(
                div()
                    .text_size(px(HEADING))
                    .line_height(px(HEADING_LINE))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Outputs"),
            );
        for (name, glyph, color) in
            [("Monitor mix", "headphones", super::ACCENT), ("Stream mix", "stream", super::STREAM)]
        {
            outputs = outputs.child(
                div()
                    .min_h(px(CONTROL))
                    .flex()
                    .items_center()
                    .gap(px(SPACE_MD))
                    .child(icon(glyph, color, ICON))
                    .child(div().flex_1().child(name))
                    .child(
                        div()
                            .text_size(px(CAPTION))
                            .line_height(px(CAPTION_LINE))
                            .text_color(rgb(MUTED))
                            .child(if active { "Ready" } else { "Stopped" }),
                    ),
            );
        }
        outputs
    }

    fn settings_overlay(&self, modal: &SettingsModal, cx: &mut Context<Self>) -> impl IntoElement {
        let device = self.snapshot.as_ref().and_then(|s| s.devices.first());
        let close = action_button(
            "close-settings",
            "Close",
            Tone::Secondary,
            true,
            cx,
            Shell::close_settings,
        )
        .track_focus(&modal.close_focus);
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x0000_00AA))
            .p(px(SPACE_XL))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("settings-panel")
                    .debug_selector(|| "settings-panel".to_owned())
                    .w(px(720.))
                    .max_h_full()
                    .overflow_y_scroll()
                    .track_focus(&modal.focus)
                    .tab_group()
                    .tab_index(0)
                    .tab_stop(false)
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                        this.close_settings(window, cx);
                        cx.stop_propagation();
                    }))
                    .p(px(SPACE_XL))
                    .rounded(px(RADIUS))
                    .bg(rgb(BACKGROUND))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_XL))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(HEADING))
                                    .line_height(px(HEADING_LINE))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Settings"),
                            )
                            .child(close),
                    )
                    .child(self.device_panel(device, cx))
                    .child(self.lifecycle_controls(cx))
                    .when_some(self.action_error.clone(), |view, error| {
                        view.child(div().text_color(rgb(WARNING)).child(error))
                    })
                    .child(self.connection_panel(cx)),
            )
    }

    fn connection_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if self.snapshot.is_some() { "Connected" } else { "Not connected" };
        div()
            .flex()
            .flex_col()
            .gap(px(SPACE_MD))
            .border_t_1()
            .border_color(rgb(BORDER))
            .pt(px(SPACE_XL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(HEADING))
                            .line_height(px(HEADING_LINE))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Connection"),
                    )
                    .child(action_button(
                        "refresh",
                        "Refresh",
                        Tone::Secondary,
                        !self.pending,
                        cx,
                        |this, _, cx| this.send(Command::GetStatus, cx),
                    )),
            )
            .child(div().flex().items_center().justify_between().child("Status").child(status))
            .child(
                div()
                    .flex()
                    .gap(px(SPACE_LG))
                    .child(div().w(px(72.)).text_color(rgb(MUTED)).child("Socket"))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(CAPTION))
                            .line_height(px(CAPTION_LINE))
                            .child(super::configured_socket_path().display().to_string()),
                    ),
            )
            .when_some(self.connection_error.clone(), |view, error| {
                view.child(div().text_color(rgb(WARNING)).child(error))
            })
    }
}
