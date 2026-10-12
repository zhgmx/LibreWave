//! Presentation of observed hardware lights. This module never reads hardware.
//!
//! The build script draws the illustration once. Lights are separate elements
//! on top of it, so a state change never redraws the illustration.
use super::theme::DANGER;
use gpui::{BoxShadow, Div, Image, ImageFormat, RenderImage, div, point, prelude::*, px, rgb};
use librewave_core::{VolumeSelection, Wave3ConfigSnapshot};
use std::sync::{Arc, LazyLock};

// Keep in step with `build.rs`.
const ART_WIDTH: u32 = 720;
const ART_HEIGHT: u32 = 1080;
const ART: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wave3.bgra"));

// The illustration's own coordinate system, from `assets/devices/wave3.svg`.
const VIEW_LEFT: f32 = 278.;
const VIEW_TOP: f32 = -7.;
const VIEW_WIDTH: f32 = 600.;
const MODE_CENTERS: [f32; 3] = [534., 576., 617.];
const MODE_Y: f32 = 447.;
const MODE_RADIUS: f32 = 3.;
const RING_CENTER: (f32, f32) = (576., 534.);
const RING_RADIUS: f32 = 29.25;
const RING_STROKE: f32 = 2.5;

const WHITE: u32 = 0x00F5_F7FF;

/// The lights that the daemon snapshot proves are lit.
///
/// The seven level dots have no verified firmware mapping, so they are never
/// lit here. The illustration shows them dark.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Lights {
    mode: Option<usize>,
    mute: Option<bool>,
}

impl Lights {
    fn observed(config: Option<&Wave3ConfigSnapshot>) -> Self {
        let Some(config) = config.filter(|config| !config.all_leds_off) else {
            return Self::default();
        };
        Self {
            mode: Some(match config.volume_select {
                VolumeSelection::Microphone => 0,
                VolumeSelection::Headphone => 1,
                VolumeSelection::Mix => 2,
            }),
            mute: Some(config.input_mute),
        }
    }

    fn ring(self) -> Option<u32> {
        self.mute.map(|muted| if muted { DANGER } else { WHITE })
    }
}

/// The illustration without lit indicators.
pub(super) fn base() -> Arc<RenderImage> {
    static BASE: LazyLock<Arc<RenderImage>> = LazyLock::new(|| {
        let frame = image::ImageBuffer::from_raw(ART_WIDTH, ART_HEIGHT, ART.to_vec())
            .expect("the build script writes one BGRA frame of the declared size");
        Arc::new(RenderImage::new(vec![image::Frame::new(frame)]))
    });
    Arc::clone(&BASE)
}

/// Lit indicators for an illustration drawn `width` wide at the origin.
pub(super) fn lights(config: Option<&Wave3ConfigSnapshot>, width: f32) -> Vec<Div> {
    let lights = Lights::observed(config);
    let scale = width / VIEW_WIDTH;
    let lamp = |x: f32, y: f32, radius: f32, color: u32| {
        div()
            .absolute()
            .left(px((x - VIEW_LEFT - radius) * scale))
            .top(px((y - VIEW_TOP - radius) * scale))
            .size(px(2. * radius * scale))
            .rounded_full()
            .shadow(vec![BoxShadow {
                color: gpui::Hsla::from(rgb(color)).opacity(0.55),
                offset: point(px(0.), px(0.)),
                blur_radius: px(10. * scale),
                spread_radius: px(0.),
            }])
    };
    let mut lit = Vec::new();
    if let Some(mode) = lights.mode {
        lit.push(lamp(MODE_CENTERS[mode], MODE_Y, MODE_RADIUS, WHITE).bg(rgb(WHITE)));
    }
    if let Some(color) = lights.ring() {
        lit.push(
            lamp(RING_CENTER.0, RING_CENTER.1, RING_RADIUS, color)
                .border(px((RING_STROKE * scale).max(1.5)))
                .border_color(rgb(color)),
        );
    }
    lit
}

/// The light that the microphone throws on the stage behind it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Glow {
    /// No observed state. The stage stays dark.
    Dark,
    Live,
    Muted,
}

impl Glow {
    pub(super) fn observed(config: Option<&Wave3ConfigSnapshot>) -> Self {
        match Lights::observed(config).mute {
            Some(true) => Self::Muted,
            Some(false) => Self::Live,
            None => Self::Dark,
        }
    }

    fn svg(self) -> String {
        let (color, strength) = match self {
            Self::Dark => ("#8fa3bf", ".05"),
            Self::Live => ("#a9c4ee", ".13"),
            Self::Muted => ("#ff4d5e", ".16"),
        };
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 200 200\"><defs>\
             <radialGradient id=\"glow\"><stop stop-color=\"{color}\" stop-opacity=\"{strength}\"/>\
             <stop offset=\".35\" stop-color=\"{color}\" stop-opacity=\".04\"/>\
             <stop offset=\"1\" stop-color=\"{color}\" stop-opacity=\"0\"/></radialGradient></defs>\
             <circle cx=\"100\" cy=\"100\" r=\"100\" fill=\"url(#glow)\"/></svg>"
        )
    }

    pub(super) fn image(self) -> Arc<Image> {
        Arc::new(Image::from_bytes(ImageFormat::Svg, self.svg().into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::FixedPointValue;

    fn config() -> Wave3ConfigSnapshot {
        let zero = FixedPointValue { raw: 0, fractional_bits: 8 };
        Wave3ConfigSnapshot {
            input_gain: zero,
            input_mute: false,
            clipguard_enable: false,
            lowcut_enable: false,
            headphone_volume: zero,
            headphone_mute: false,
            direct_monitor: zero,
            volume_select: VolumeSelection::Microphone,
            all_leds_off: false,
            leds_flip: false,
            gain_lock: false,
        }
    }

    #[test]
    fn illustration_frame_has_the_declared_size() {
        assert_eq!(ART.len(), (ART_WIDTH * ART_HEIGHT * 4) as usize);
        // The capsule is opaque at its center and the canvas corner is clear.
        let alpha = |x: u32, y: u32| ART[((y * ART_WIDTH + x) * 4 + 3) as usize];
        assert_eq!(alpha(ART_WIDTH / 2, ART_HEIGHT / 4), 255);
        assert_eq!(alpha(0, 0), 0);
    }

    #[test]
    fn ring_tracks_hardware_microphone_mute_only() {
        let mut config = config();
        config.headphone_mute = true;
        assert_eq!(Lights::observed(Some(&config)).ring(), Some(WHITE));
        config.input_mute = true;
        assert_eq!(Lights::observed(Some(&config)).ring(), Some(DANGER));
    }

    #[test]
    fn stage_glow_follows_the_observed_ring_and_stays_dark_without_state() {
        let mut config = config();
        assert_eq!(Glow::observed(Some(&config)), Glow::Live);
        config.input_mute = true;
        assert_eq!(Glow::observed(Some(&config)), Glow::Muted);
        config.all_leds_off = true;
        assert_eq!(Glow::observed(Some(&config)), Glow::Dark);
        assert_eq!(Glow::observed(None), Glow::Dark);
    }

    #[test]
    fn unavailable_state_and_leds_off_do_not_invent_lit_indicators() {
        let mut config = config();
        config.input_mute = true;
        config.all_leds_off = true;
        for observed in [None, Some(&config)] {
            assert_eq!(Lights::observed(observed), Lights::default());
            assert!(lights(observed, 360.).is_empty());
        }
    }

    #[test]
    fn each_knob_target_lights_only_its_mode_indicator() {
        for (index, target) in
            [VolumeSelection::Microphone, VolumeSelection::Headphone, VolumeSelection::Mix]
                .into_iter()
                .enumerate()
        {
            let mut config = config();
            config.volume_select = target;
            assert_eq!(Lights::observed(Some(&config)).mode, Some(index));
            // One mode lamp and the ring. No level dot is ever lit.
            assert_eq!(lights(Some(&config), 360.).len(), 2);
        }
    }
}
