//! Presentation of observed hardware lights. This module never reads hardware.
//!
//! The build script draws the illustration once. Lights are separate elements
//! above it, with the opaque knob face in front of the ring. A state change
//! never redraws the illustration.
use super::theme::DANGER;
use gpui::{
    AnyElement, BoxShadow, Image, ImageFormat, RenderImage, div, img, point, prelude::*, px, rgb,
};
use librewave_core::{ApiVersion, VolumeSelection, Wave3ConfigSnapshot};
use std::sync::{Arc, LazyLock};

// Keep in step with `build.rs`.
const ART_WIDTH: u32 = 720;
const ART_HEIGHT: u32 = 1080;
const ART: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wave3.bgra"));
const KNOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wave3-knob.bgra"));
include!(concat!(env!("OUT_DIR"), "/wave3-knob.rs"));

// The illustration's own coordinate system, from `assets/devices/wave3.svg`.
const VIEW_LEFT: f32 = 278.;
const VIEW_TOP: f32 = -7.;
const VIEW_WIDTH: f32 = 600.;
const MODE_CENTERS: [f32; 3] = [534., 576., 617.];
const MODE_Y: f32 = 447.;
const MODE_RADIUS: f32 = 3.;
const LEVEL_CENTERS: [f32; 7] = [534., 548., 562., 576., 589., 603., 617.];
const LEVEL_Y: f32 = 483.;
const RING_CENTER: (f32, f32) = (576., 534.);
const RING_RADIUS: f32 = 29.25;
const RING_STROKE: f32 = 2.5;

const WHITE: u32 = 0x00F5_F7FF;

/// The lights that the daemon snapshot proves are lit.
///
/// Level counts are available only inside verified Microphone-mode regions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Lights {
    mode: Option<usize>,
    mute: Option<bool>,
    levels: Option<usize>,
}

impl Lights {
    fn observed(config: Option<&Wave3ConfigSnapshot>, api: Option<ApiVersion>) -> Self {
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
            levels: microphone_level_count(config, api),
        }
    }

    fn ring(self) -> Option<u32> {
        self.mute.map(|muted| if muted { DANGER } else { WHITE })
    }
}

/// Measured Q8.8 transition brackets for Wave:3 API 5.4. The lower endpoint
/// retained the previous count; the upper endpoint showed the next count.
const MICROPHONE_TRANSITIONS: [(i32, i32); 7] = [
    (640, 768),
    (2176, 2304),
    (3712, 3840),
    (5248, 5376),
    (6784, 6912),
    (8320, 8448),
    (10112, 10240),
];

fn microphone_level_count(config: &Wave3ConfigSnapshot, api: Option<ApiVersion>) -> Option<usize> {
    if api != Some(ApiVersion::new(5, 4))
        || config.volume_select != VolumeSelection::Microphone
        || config.input_mute
        || config.all_leds_off
        || config.leds_flip
        || config.input_gain.fractional_bits != 8
        || !(0..=10240).contains(&config.input_gain.raw)
    {
        return None;
    }
    let gain = config.input_gain.raw;
    if MICROPHONE_TRANSITIONS.iter().any(|&(lower, upper)| lower < gain && gain < upper) {
        return None;
    }
    Some(MICROPHONE_TRANSITIONS.iter().filter(|&&(_, upper)| gain >= upper).count())
}

pub(super) fn level_hint(
    config: Option<&Wave3ConfigSnapshot>,
    api: Option<ApiVersion>,
) -> Option<&'static str> {
    config
        .filter(|config| !config.all_leds_off && microphone_level_count(config, api).is_none())
        .map(|_| "Level indicator mapping is not verified for this state.")
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

/// The same SVG cap, placed above the dynamic ring so its opaque face masks
/// the inner glow. Its geometry and raster are generated together at build time.
pub(super) fn knob_face(width: f32) -> impl gpui::IntoElement {
    static FACE: LazyLock<Arc<RenderImage>> = LazyLock::new(|| {
        let frame = image::ImageBuffer::from_raw(KNOB_PIXELS.0, KNOB_PIXELS.1, KNOB.to_vec())
            .expect("the build writes a knob frame of the declared size");
        Arc::new(RenderImage::new(vec![image::Frame::new(frame)]))
    });
    let scale = width / VIEW_WIDTH;
    div()
        .id("knob-face")
        .debug_selector(|| "knob-face".to_owned())
        .absolute()
        .left(px(KNOB_BOUNDS.0 * scale))
        .top(px(KNOB_BOUNDS.1 * scale))
        .w(px(KNOB_BOUNDS.2 * scale))
        .h(px(KNOB_BOUNDS.3 * scale))
        .child(img(Arc::clone(&FACE)).size_full())
}

/// Lit indicators for an illustration drawn `width` wide at the origin.
pub(super) fn lights(
    config: Option<&Wave3ConfigSnapshot>,
    api: Option<ApiVersion>,
    width: f32,
) -> Vec<AnyElement> {
    let lights = Lights::observed(config, api);
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
        lit.push(
            lamp(MODE_CENTERS[mode], MODE_Y, MODE_RADIUS, WHITE).bg(rgb(WHITE)).into_any_element(),
        );
    }
    if let Some(count) = lights.levels {
        for (index, &center) in LEVEL_CENTERS.iter().take(count).enumerate() {
            let selector = format!("device-level-{index}");
            lit.push(
                lamp(center, LEVEL_Y, MODE_RADIUS, WHITE)
                    .bg(rgb(WHITE))
                    .id(gpui::SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .into_any_element(),
            );
        }
    }
    if let Some(color) = lights.ring() {
        lit.push(
            lamp(RING_CENTER.0, RING_CENTER.1, RING_RADIUS, color)
                .border(px((RING_STROKE * scale).max(1.5)))
                .border_color(rgb(color))
                .id("knob-ring")
                .debug_selector(|| "knob-ring".to_owned())
                .into_any_element(),
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
        match Lights::observed(config, None).mute {
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
    fn foreground_cap_has_opaque_center_and_clear_corners() {
        assert_eq!(KNOB.len(), (KNOB_PIXELS.0 * KNOB_PIXELS.1 * 4) as usize);
        let alpha = |x: u32, y: u32| KNOB[((y * KNOB_PIXELS.0 + x) * 4 + 3) as usize];
        assert_eq!(alpha(KNOB_PIXELS.0 / 2, KNOB_PIXELS.1 / 2), 255);
        assert_eq!(alpha(0, 0), 0);
        // The circular face shares the ring center. Integer raster dimensions
        // add less than one image pixel of transparent padding at the edge.
        let center_x = KNOB_BOUNDS.0 + KNOB_BOUNDS.2 / 2.;
        let center_y = KNOB_BOUNDS.1 + KNOB_BOUNDS.3 / 2.;
        assert!((center_x - (RING_CENTER.0 - VIEW_LEFT)).abs() < 0.5);
        assert!((center_y - (RING_CENTER.1 - VIEW_TOP)).abs() < 0.5);
        assert!((KNOB_BOUNDS.2 - KNOB_BOUNDS.3).abs() < f32::EPSILON);
        assert!(KNOB_BOUNDS.2 < 2. * RING_RADIUS);
    }

    #[test]
    fn microphone_counts_match_every_physical_observation() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/wave3-api5-4-microphone-led-counts.json"
        ))
        .expect("checked-in physical observations");
        let mut config = config();
        let observations = fixture["observations"].as_array().expect("observations");
        let edge = fixture["edge_verification"].as_array().expect("seventh-dot confirmations");
        assert_eq!(observations.len(), 47);
        assert_eq!(edge.len(), 12);
        for observation in observations.iter().chain(edge) {
            config.input_gain.raw =
                i32::try_from(observation["gain_raw"].as_i64().expect("raw gain"))
                    .expect("Q8.8 gain fits");
            let count = usize::try_from(
                observation
                    .get("lit_count")
                    .unwrap_or(&observation["fully_lit_count"])
                    .as_u64()
                    .expect("count"),
            )
            .expect("seven LEDs fit");
            assert_eq!(microphone_level_count(&config, Some(ApiVersion::new(5, 4))), Some(count));
            assert_eq!(lights(Some(&config), Some(ApiVersion::new(5, 4)), 360.).len(), count + 2);
            if let Some(last_led) = observation.get("last_led") {
                assert_eq!(last_led == "fully_lit", count == 7);
                assert!(last_led == "off" || last_led == "fully_lit");
            }
        }
    }

    #[test]
    fn fractional_transition_regions_are_unknown_without_rounding_the_gain() {
        let mut config = config();
        for &(lower, upper) in &MICROPHONE_TRANSITIONS {
            config.input_gain.raw = lower;
            let before = microphone_level_count(&config, Some(ApiVersion::new(5, 4)))
                .expect("lower tested endpoint");
            for raw in lower + 1..upper {
                config.input_gain.raw = raw;
                assert_eq!(microphone_level_count(&config, Some(ApiVersion::new(5, 4))), None);
                assert!(level_hint(Some(&config), Some(ApiVersion::new(5, 4))).is_some());
            }
            config.input_gain.raw = upper;
            assert_eq!(
                microphone_level_count(&config, Some(ApiVersion::new(5, 4))),
                Some(before + 1)
            );
        }
        for raw in [3938, 4726] {
            config.input_gain.raw = raw;
            assert_eq!(microphone_level_count(&config, Some(ApiVersion::new(5, 4))), Some(3));
        }
    }

    #[test]
    fn unverified_modes_versions_and_muted_or_flipped_patterns_stay_unknown() {
        let mut config = config();
        config.input_gain.raw = 10240;
        for api in [None, Some(ApiVersion::new(5, 3)), Some(ApiVersion::new(5, 5))] {
            assert_eq!(microphone_level_count(&config, api), None);
        }
        for target in [VolumeSelection::Headphone, VolumeSelection::Mix] {
            config.volume_select = target;
            assert_eq!(microphone_level_count(&config, Some(ApiVersion::new(5, 4))), None);
        }
        config.volume_select = VolumeSelection::Microphone;
        for mutation in [
            |config: &mut Wave3ConfigSnapshot| config.input_mute = true,
            |config: &mut Wave3ConfigSnapshot| config.all_leds_off = true,
            |config: &mut Wave3ConfigSnapshot| config.leds_flip = true,
            |config: &mut Wave3ConfigSnapshot| config.input_gain.fractional_bits = 7,
            |config: &mut Wave3ConfigSnapshot| config.input_gain.raw = -1,
            |config: &mut Wave3ConfigSnapshot| config.input_gain.raw = 10241,
        ] {
            let mut changed = config.clone();
            mutation(&mut changed);
            assert_eq!(microphone_level_count(&changed, Some(ApiVersion::new(5, 4))), None);
        }
    }

    #[test]
    fn ring_tracks_hardware_microphone_mute_only() {
        let mut config = config();
        config.headphone_mute = true;
        assert_eq!(
            Lights::observed(Some(&config), Some(ApiVersion::new(5, 4))).ring(),
            Some(WHITE)
        );
        config.input_mute = true;
        assert_eq!(
            Lights::observed(Some(&config), Some(ApiVersion::new(5, 4))).ring(),
            Some(DANGER)
        );
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
            assert_eq!(Lights::observed(observed, Some(ApiVersion::new(5, 4))), Lights::default());
            assert!(lights(observed, Some(ApiVersion::new(5, 4)), 360.).is_empty());
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
            assert_eq!(
                Lights::observed(Some(&config), Some(ApiVersion::new(5, 4))).mode,
                Some(index)
            );
            // Zero gain has no lit level dots. Other modes have no verified mapping.
            assert_eq!(lights(Some(&config), Some(ApiVersion::new(5, 4)), 360.).len(), 2);
        }
    }
}
