use super::{BACKGROUND, BORDER, INK, MUTED, PANEL, RAISED, SettingsView, Shell, WARNING, button};
use gpui::{
    Context, FontWeight, IntoElement, SharedString, Svg, div, prelude::*, px, rgb, rgba, svg,
};
use librewave_core::{Command, MixerRuntimeState};

pub(super) fn icon(name: &'static str, color: u32, extent: f32) -> Svg {
    svg().path(name).size(px(extent)).text_color(rgb(color))
}

impl Shell {
    pub(super) fn mixer_frame(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mixer = div()
            .w_full()
            .h(px(850.))
            .max_h_full()
            .max_w(px(1280.))
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BACKGROUND))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(self.titlebar(cx))
            .child(
                div()
                    .id("mixer-canvas")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.mixer_channels(cx))
                    .child(self.output_rows()),
            );
        div()
            .size_full()
            .relative()
            .p(px(32.))
            .bg(rgb(0x0000_0000))
            .text_color(rgb(INK))
            .font_family("Geist")
            .text_size(px(14.))
            .flex()
            .items_center()
            .justify_center()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.settings_view != SettingsView::Closed {
                    this.settings_view = SettingsView::Closed;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(mixer)
            .when(self.settings_view != SettingsView::Closed, |view| {
                view.child(self.settings_overlay(cx))
            })
    }

    fn titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state =
            match (self.action_error.is_some(), self.snapshot.as_ref().map(|s| s.mixer.runtime)) {
                (true, _) => "Needs attention",
                (false, None) => "Not connected",
                (false, Some(MixerRuntimeState::Inactive { .. })) => "Audio stopped",
                (false, Some(MixerRuntimeState::Active)) => "Audio running",
            };
        let mut mark = div().h(px(24.)).flex().items_center().gap(px(3.));
        for height in [12., 20., 24., 20., 12.] {
            mark = mark.child(div().w(px(3.)).h(px(height)).rounded(px(2.)).bg(rgb(INK)));
        }
        div()
            .h(px(64.))
            .flex_shrink_0()
            .px(px(24.))
            .border_b_1()
            .border_color(rgb(BORDER))
            .flex()
            .items_center()
            .justify_between()
            .child(div().flex().items_center().gap(px(12.)).child(mark).child(
                div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child("LibreWave"),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(state))
                    .child(
                        div()
                            .id("settings")
                            .size(px(32.))
                            .rounded(px(6.))
                            .focusable()
                            .focus_visible(|s| s.bg(rgb(RAISED)))
                            .hover(|s| s.bg(rgb(RAISED)))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("settings", MUTED, 20.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settings_view = SettingsView::Open;
                                cx.notify();
                            }))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.settings_view = SettingsView::Open;
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            })),
                    ),
            )
    }

    fn output_rows(&self) -> impl IntoElement {
        let connected = self.snapshot.is_some();
        let mut outputs = div()
            .p(px(24.))
            .border_t_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .h(px(32.))
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Outputs"),
            );
        for (name, glyph, color) in
            [("Monitor mix", "headphones", super::ACCENT), ("Stream mix", "stream", super::STREAM)]
        {
            outputs = outputs.child(
                div()
                    .h(px(48.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(icon(glyph, if connected { color } else { MUTED }, 20.))
                    .child(
                        div()
                            .w(px(200.))
                            .h(px(32.))
                            .px(px(8.))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded(px(6.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_size(px(12.)).child(name))
                            .child(icon("chevron", MUTED, 12.)),
                    )
                    .child(div().flex_1().h(px(4.)).rounded(px(2.)).bg(rgb(BORDER)))
                    .child(icon("speaker", MUTED, 20.))
                    .child(div().w(px(128.)).h(px(4.)).rounded(px(2.)).bg(rgb(BORDER)))
                    .child(div().w(px(36.)).text_size(px(12.)).text_color(rgb(MUTED)).child("—"))
                    .child(icon("listen", MUTED, 20.)),
            );
        }
        outputs
    }

    fn settings_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let device = self.snapshot.as_ref().and_then(|s| s.devices.first());
        div()
            .absolute()
            .inset_0()
            .bg(rgba(0x0000_0099))
            .p(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("settings-panel")
                    .w(px(620.))
                    .max_h_full()
                    .overflow_y_scroll()
                    .p(px(24.))
                    .rounded(px(6.))
                    .bg(rgb(BACKGROUND))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Settings"),
                            )
                            .child(button("close-settings", "Close").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.settings_view = SettingsView::Closed;
                                    cx.notify();
                                },
                            ))),
                    )
                    .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child("Additional inputs, effects, linked levels, live meters and output master controls are not available yet."))
                .child(self.lifecycle_controls(cx))
                    .when_some(self.action_error.clone(), |view, error| {
                        view.child(div().text_color(rgb(WARNING)).child(error))
                    })
                    .child(
                        button("refresh", "Refresh status").on_click(
                            cx.listener(|this, _, _, cx| this.send(Command::GetStatus, cx)),
                        ),
                    )
                    .child(self.device_panel(device, cx))
                    .child(
                        button(
                            "connection-details",
                            if self.settings_view == SettingsView::ConnectionDetails {
                                "Hide connection details"
                            } else {
                                "Connection details"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings_view = if this.settings_view == SettingsView::ConnectionDetails { SettingsView::Open } else { SettingsView::ConnectionDetails };
                            cx.notify();
                        })),
                    )
                    .when(self.settings_view == SettingsView::ConnectionDetails, |view| {
                        view.child(
                            div()
                                .p(px(12.))
                                .rounded(px(6.))
                                .bg(rgb(PANEL))
                                .text_size(px(12.))
                                .text_color(rgb(MUTED))
                                .child(SharedString::from(
                                    self.connection_error.clone().unwrap_or_else(|| {
                                        "Connected to the local daemon.".to_owned()
                                    }),
                                )),
                        )
                    }),
            )
    }
}
