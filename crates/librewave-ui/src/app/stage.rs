//! The device stage: the microphone illustration with its hardware controls.
use super::controls::{Tone, dot, icon_button, tip};
use super::device_art::{self, Glow};
use super::theme::{
    BODY, BODY_LINE, DISPLAY, DISPLAY_LINE, FAINT, INK, MONITOR, MUTED, SPACE_SM, SPACE_XL,
    SPACE_XS,
};
use super::{Shell, TITLEBAR};
use gpui::{Context, Div, FontWeight, IntoElement, div, img, prelude::*, px, rgb};
use librewave_core::{ApiVersion, Command, DeviceConnection, DeviceSnapshot, Wave3ConfigSnapshot};

pub(super) const STAGE_WIDTH: f32 = 420.;
const PADDING: f32 = 28.;
// The illustration is drawn on a 2:3 canvas.
const ART_MAX: f32 = 460.;
const ART_MIN: f32 = 170.;
const ART_COMPACT: f32 = 240.;
// Title bar, stage padding, heading, hardware controls, and the gaps between them.
const STAGE_FIXED: f32 = TITLEBAR + 2. * PADDING + 64. + 220. + 2. * SPACE_XL;

impl Shell {
    pub(super) fn stage(
        &self,
        compact: bool,
        viewport_height: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let device = self.snapshot.as_ref().and_then(|snapshot| snapshot.devices.first());
        let connected =
            device.is_some_and(|device| device.connection == DeviceConnection::Connected);
        let config = device.filter(|_| connected).and_then(|device| device.admission.config());
        let art_height =
            if compact { ART_COMPACT } else { art_height_for_viewport(viewport_height) };
        let api = device.and_then(|device| device.admission.admitted_api());
        let art = artwork(config, api, connected, art_height);
        let heading = self.stage_heading(device, config.is_some(), cx);
        let controls = self.hardware_controls(device, cx);
        let stage = div()
            .id("device-stage")
            .debug_selector(|| "device-stage".to_owned())
            .flex_shrink_0()
            .p(px(PADDING))
            .flex()
            .gap(px(SPACE_XL));
        if compact {
            stage.w_full().items_center().child(art).child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_XL))
                    .child(heading)
                    .child(controls),
            )
        } else {
            stage
                .w(px(STAGE_WIDTH))
                .h_full()
                .flex_col()
                .child(heading)
                .child(div().flex_1().flex().items_center().justify_center().child(art))
                .child(controls)
        }
    }

    fn stage_heading(
        &self,
        device: Option<&DeviceSnapshot>,
        loaded: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let (color, state) = match (self.snapshot.is_some(), device.map(|device| device.connection))
        {
            (false, _) => (FAINT, "Offline".to_owned()),
            (true, None) => (FAINT, "Not connected".to_owned()),
            (true, Some(DeviceConnection::Disconnected)) => (FAINT, "Disconnected".to_owned()),
            (true, Some(DeviceConnection::Connected)) => (
                MONITOR,
                device
                    .and_then(|device| device.admission.admitted_api())
                    .map_or_else(|| "Connected".to_owned(), |api| format!("Connected · API {api}")),
            ),
        };
        let title = div()
            .flex()
            .flex_col()
            .gap(px(SPACE_XS))
            .child(
                div()
                    .text_size(px(DISPLAY))
                    .line_height(px(DISPLAY_LINE))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(if device.is_some() { INK } else { MUTED }))
                    .child("Wave:3"),
            )
            .child(
                div().flex().items_center().gap(px(SPACE_SM)).child(dot(color)).child(
                    div()
                        .text_size(px(BODY))
                        .line_height(px(BODY_LINE))
                        .text_color(rgb(MUTED))
                        .child(state),
                ),
            );
        let row = div().w_full().flex().items_start().justify_between().child(title);
        // A loaded device keeps the read action as one quiet icon.
        match device.filter(|_| loaded) {
            Some(device) => {
                let id = device.id;
                row.child(icon_button(
                    "inspect",
                    "arrow-clockwise",
                    "Read controls again. Reading may restore saved device settings.",
                    Tone::Quiet,
                    MUTED,
                    !self.pending && self.settings_modal.is_none(),
                    cx,
                    move |this, _, cx| this.send(Command::InspectDevice { id }, cx),
                ))
            }
            None => row,
        }
    }
}

fn art_height_for_viewport(viewport_height: f32) -> f32 {
    (viewport_height - STAGE_FIXED).clamp(ART_MIN, ART_MAX)
}

fn artwork(
    config: Option<&Wave3ConfigSnapshot>,
    api: Option<ApiVersion>,
    connected: bool,
    height: f32,
) -> impl IntoElement {
    let width = height * 2. / 3.;
    // The light is centered on the capsule. Nothing clips it, so it fades out
    // over the stage and the mixer behind their panels.
    let glow = height * 2.6;
    div()
        .id("device-artwork")
        .w(px(width))
        .h(px(height))
        .flex_shrink_0()
        .relative()
        .child(
            img(Glow::observed(config).image())
                .absolute()
                .left(px((width - glow) / 2.))
                .top(px(height * 0.38 - glow / 2.))
                .size(px(glow)),
        )
        .child(
            div()
                .absolute()
                .size_full()
                .when(!connected, |view| view.opacity(0.35))
                .child(img(device_art::base()).size_full())
                .children(device_art::lights(config, api, width))
                .child(device_art::knob_face(width)),
        )
        .when_some(device_art::level_hint(config, api), |view, hint| view.tooltip(tip(hint)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_fits_the_shortest_window_and_stops_growing_on_tall_ones() {
        assert!((art_height_for_viewport(700.) - (700. - STAGE_FIXED)).abs() < f32::EPSILON);
        assert!((art_height_for_viewport(100.) - ART_MIN).abs() < f32::EPSILON);
        assert!((art_height_for_viewport(4000.) - ART_MAX).abs() < f32::EPSILON);
    }
}
