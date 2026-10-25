//! Presentation of observed hardware lights. This module never reads hardware.
//!
//! The build script draws the illustration once. Lights are separate elements
//! above it, with the opaque knob face in front of the ring. A state change
//! never redraws the illustration.
use super::theme::DANGER;
use gpui::{AnyElement, BoxShadow, Image, ImageFormat, RenderImage, div, img, prelude::*, px, rgb};
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
/// Level positions are available only inside physically verified regions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Lights {
    mode: Option<usize>,
    mute: Option<bool>,
    levels: Option<[bool; 7]>,
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
            levels: level_pattern(config, api),
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

/// Physical Headphone-mode transition brackets on the 0.5 dB write grid.
const HEADPHONE_TRANSITIONS: [(i32, i32); 7] = [
    (-13952, -13824),
    (-6656, -6528),
    (-4352, -4224),
    (-2816, -2688),
    (-1664, -1536),
    (-896, -768),
    (-128, 0),
];

// One LED per measured 5-percent value, indexed from left to right. Blend
// increases from the rightmost LED at 0 percent to the leftmost at 100 percent.
const MIX_POSITIONS: [usize; 21] = [6, 5, 5, 5, 4, 4, 4, 3, 3, 3, 3, 2, 2, 2, 1, 1, 1, 0, 0, 0, 0];

fn level_pattern(config: &Wave3ConfigSnapshot, api: Option<ApiVersion>) -> Option<[bool; 7]> {
    let mut pattern = match config.volume_select {
        VolumeSelection::Microphone => microphone_level_count(config, api).map(filled),
        VolumeSelection::Headphone => headphone_level_count(config, api).map(filled),
        VolumeSelection::Mix => mix_level_pattern(config, api),
    }?;
    // LED flip mirrors the seven level LEDs for an upside-down microphone.
    if config.leds_flip {
        pattern.reverse();
    }
    Some(pattern)
}

fn filled(count: usize) -> [bool; 7] {
    std::array::from_fn(|index| index < count)
}

fn mix_level_pattern(config: &Wave3ConfigSnapshot, api: Option<ApiVersion>) -> Option<[bool; 7]> {
    if api != Some(ApiVersion::new(5, 4))
        || config.volume_select != VolumeSelection::Mix
        || config.all_leds_off
        || config.direct_monitor.fractional_bits != 8
        || !(0..=25600).contains(&config.direct_monitor.raw)
        || config.direct_monitor.raw % 1280 != 0
    {
        return None;
    }
    let step = usize::try_from(config.direct_monitor.raw / 1280).ok()?;
    let position = *MIX_POSITIONS.get(step)?;
    Some(std::array::from_fn(|index| index == position))
}

fn headphone_level_count(config: &Wave3ConfigSnapshot, api: Option<ApiVersion>) -> Option<usize> {
    if api != Some(ApiVersion::new(5, 4))
        || config.volume_select != VolumeSelection::Headphone
        || config.all_leds_off
        || config.headphone_volume.fractional_bits != 8
    {
        return None;
    }
    if config.headphone_mute {
        return Some(0);
    }
    bracketed_count(config.headphone_volume.raw, -15360, 0, &HEADPHONE_TRANSITIONS)
}

fn bracketed_count(
    raw: i32,
    minimum: i32,
    maximum: i32,
    transitions: &[(i32, i32)],
) -> Option<usize> {
    if !(minimum..=maximum).contains(&raw)
        || transitions.iter().any(|&(lower, upper)| lower < raw && raw < upper)
    {
        return None;
    }
    Some(transitions.iter().filter(|&&(_, upper)| raw >= upper).count())
}

fn microphone_level_count(config: &Wave3ConfigSnapshot, api: Option<ApiVersion>) -> Option<usize> {
    if api != Some(ApiVersion::new(5, 4))
        || config.volume_select != VolumeSelection::Microphone
        || config.all_leds_off
        || config.input_gain.fractional_bits != 8
    {
        return None;
    }
    bracketed_count(config.input_gain.raw, 0, 10240, &MICROPHONE_TRANSITIONS)
}

pub(super) fn level_hint(
    config: Option<&Wave3ConfigSnapshot>,
    api: Option<ApiVersion>,
) -> Option<&'static str> {
    config
        .filter(|config| !config.all_leds_off && level_pattern(config, api).is_none())
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
            .shadow(vec![
                BoxShadow::new(px(0.), px(0.), gpui::Hsla::from(rgb(color)).opacity(0.55))
                    .blur_radius(px(10. * scale)),
            ])
    };
    let mut lit = Vec::new();
    if let Some(mode) = lights.mode {
        lit.push(
            lamp(MODE_CENTERS[mode], MODE_Y, MODE_RADIUS, WHITE).bg(rgb(WHITE)).into_any_element(),
        );
    }
    if let Some(pattern) = lights.levels {
        for (index, &center) in
            LEVEL_CENTERS.iter().enumerate().filter(|(index, _)| pattern[*index])
        {
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

    /// The light color and its opacity at the center.
    const fn light(self) -> (u32, f32) {
        match self {
            Self::Dark => (0x008F_A3BF, 0.05),
            Self::Live => (0x00A9_C4EE, 0.13),
            Self::Muted => (0x00FF_4D5E, 0.16),
        }
    }

    /// The radial opacity stops: the center, the shoulder, and the rim.
    const SHOULDER: f32 = 0.35;
    const SHOULDER_OPACITY: f32 = 0.04;

    fn svg(self) -> String {
        let (color, strength) = self.light();
        let (shoulder, shoulder_opacity) = (Self::SHOULDER, Self::SHOULDER_OPACITY);
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 200 200\"><defs>\
             <radialGradient id=\"glow\"><stop stop-color=\"#{color:06x}\" stop-opacity=\"{strength}\"/>\
             <stop offset=\"{shoulder}\" stop-color=\"#{color:06x}\" stop-opacity=\"{shoulder_opacity}\"/>\
             <stop offset=\"1\" stop-color=\"#{color:06x}\" stop-opacity=\"0\"/></radialGradient></defs>\
             <circle cx=\"100\" cy=\"100\" r=\"100\" fill=\"url(#glow)\"/></svg>"
        )
    }

    /// The color that the glow, drawn in `area`, leaves over `background` at
    /// `at`. Edge fades end in this color, so they meet the lit backdrop.
    pub(super) fn backdrop(
        self,
        area: gpui::Bounds<gpui::Pixels>,
        at: gpui::Point<gpui::Pixels>,
        background: u32,
    ) -> gpui::Rgba {
        let (color, strength) = self.light();
        let radius = f32::from(area.size.width) / 2.;
        let center = area.center();
        let (dx, dy) = (f32::from(at.x - center.x), f32::from(at.y - center.y));
        let distance = if radius > 0. { dx.hypot(dy) / radius } else { f32::INFINITY };
        let opacity = if distance < Self::SHOULDER {
            strength + (Self::SHOULDER_OPACITY - strength) * distance / Self::SHOULDER
        } else if distance < 1. {
            Self::SHOULDER_OPACITY * (1. - distance) / (1. - Self::SHOULDER)
        } else {
            0.
        };
        let channel = |value: u32, shift: u32| {
            f32::from(u8::try_from((value >> shift) & 0xFF).unwrap_or(0)) / 255.
        };
        let mix =
            |shift| channel(background, shift) * (1. - opacity) + channel(color, shift) * opacity;
        gpui::Rgba { r: mix(16), g: mix(8), b: mix(0), a: 1. }
    }

    /// The glow image. Each state is built once and reused by every frame.
    pub(super) fn image(self) -> Arc<Image> {
        static IMAGES: LazyLock<[Arc<Image>; 3]> = LazyLock::new(|| {
            [Glow::Dark, Glow::Live, Glow::Muted]
                .map(|glow| Arc::new(Image::from_bytes(ImageFormat::Svg, glow.svg().into_bytes())))
        });
        Arc::clone(&IMAGES[self as usize])
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
    fn headphone_positions_match_every_physical_observation() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/wave3-api5-4-headphone-led-counts.json"
        ))
        .expect("checked-in physical observations");
        let mut config = config();
        config.volume_select = VolumeSelection::Headphone;
        let observations = fixture["observations"].as_array().expect("observations");
        assert_eq!(observations.len(), 50);
        for observation in observations {
            config.headphone_volume.raw =
                i32::try_from(observation["headphone_raw"].as_i64().expect("raw level"))
                    .expect("Q8.8 headphone level fits");
            let count = usize::try_from(observation["lit_count"].as_u64().expect("count"))
                .expect("seven LEDs fit");
            let pattern = level_pattern(&config, Some(ApiVersion::new(5, 4)))
                .expect("observed headphone pattern");
            let positions: Vec<_> = pattern
                .iter()
                .enumerate()
                .filter_map(|(index, lit)| lit.then_some(index + 1))
                .collect();
            let observed_positions: Vec<_> = observation["positions"]
                .as_array()
                .expect("positions")
                .iter()
                .map(|position| usize::try_from(position.as_u64().expect("position")).expect("LED"))
                .collect();
            assert_eq!(positions, observed_positions);
            assert_eq!(headphone_level_count(&config, Some(ApiVersion::new(5, 4))), Some(count));
            assert_eq!(lights(Some(&config), Some(ApiVersion::new(5, 4)), 360.).len(), count + 2);
        }
    }

    #[test]
    fn headphone_fractional_brackets_and_unverified_states_stay_unknown() {
        let mut config = config();
        config.volume_select = VolumeSelection::Headphone;
        for &(lower, upper) in &HEADPHONE_TRANSITIONS {
            for raw in lower + 1..upper {
                config.headphone_volume.raw = raw;
                assert_eq!(headphone_level_count(&config, Some(ApiVersion::new(5, 4))), None);
                assert!(level_hint(Some(&config), Some(ApiVersion::new(5, 4))).is_some());
            }
        }
        config.headphone_volume.raw = -1152;
        for mutation in [
            |config: &mut Wave3ConfigSnapshot| config.all_leds_off = true,
            |config: &mut Wave3ConfigSnapshot| config.headphone_volume.fractional_bits = 7,
            |config: &mut Wave3ConfigSnapshot| config.headphone_volume.raw = -15361,
            |config: &mut Wave3ConfigSnapshot| config.headphone_volume.raw = 1,
        ] {
            let mut changed = config.clone();
            mutation(&mut changed);
            assert_eq!(headphone_level_count(&changed, Some(ApiVersion::new(5, 4))), None);
        }
        for api in [None, Some(ApiVersion::new(5, 3)), Some(ApiVersion::new(5, 5))] {
            assert_eq!(headphone_level_count(&config, api), None);
        }
    }

    #[test]
    fn mix_positions_match_every_physical_observation() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/wave3-api5-4-blend-led-positions.json"
        ))
        .expect("checked-in physical observations");
        let mut config = config();
        config.volume_select = VolumeSelection::Mix;
        let observations = fixture["observations"].as_array().expect("observations");
        assert_eq!(observations.len(), 33);
        for observation in observations {
            config.direct_monitor.raw =
                i32::try_from(observation["blend_raw"].as_i64().expect("raw blend"))
                    .expect("Q8.8 blend fits");
            let pattern =
                level_pattern(&config, Some(ApiVersion::new(5, 4))).expect("observed Mix pattern");
            let positions: Vec<_> = pattern
                .iter()
                .enumerate()
                .filter_map(|(index, lit)| lit.then_some(index + 1))
                .collect();
            let observed_positions: Vec<_> = observation["positions"]
                .as_array()
                .expect("positions")
                .iter()
                .map(|position| usize::try_from(position.as_u64().expect("position")).expect("LED"))
                .collect();
            assert_eq!(positions, observed_positions);
            assert_eq!(positions.len(), 1, "Mix moves one dot instead of filling a bar");
            assert_eq!(lights(Some(&config), Some(ApiVersion::new(5, 4)), 360.).len(), 3);
        }
    }

    #[test]
    fn mix_unmeasured_values_and_unverified_states_stay_unknown() {
        let mut config = config();
        config.volume_select = VolumeSelection::Mix;
        for raw in 0..=25600 {
            config.direct_monitor.raw = raw;
            assert_eq!(
                mix_level_pattern(&config, Some(ApiVersion::new(5, 4))).is_some(),
                raw % 1280 == 0
            );
        }
        config.direct_monitor.raw = 12800;
        for mutation in [
            |config: &mut Wave3ConfigSnapshot| config.all_leds_off = true,
            |config: &mut Wave3ConfigSnapshot| config.direct_monitor.fractional_bits = 7,
            |config: &mut Wave3ConfigSnapshot| config.direct_monitor.raw = -1,
            |config: &mut Wave3ConfigSnapshot| config.direct_monitor.raw = 25601,
        ] {
            let mut changed = config.clone();
            mutation(&mut changed);
            assert_eq!(mix_level_pattern(&changed, Some(ApiVersion::new(5, 4))), None);
        }
        for api in [None, Some(ApiVersion::new(5, 3)), Some(ApiVersion::new(5, 5))] {
            assert_eq!(mix_level_pattern(&config, api), None);
        }
    }

    #[test]
    fn unverified_modes_and_versions_stay_unknown() {
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
            |config: &mut Wave3ConfigSnapshot| config.all_leds_off = true,
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
    fn observed_mute_keeps_the_three_left_microphone_leds_at_16_db() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/wave3-api5-4-mute-led-positions.json"
        ))
        .expect("checked-in physical mute observations");
        let mut config = config();
        config.input_gain.raw = 4096;
        let observations = fixture["observations"].as_array().expect("observations");
        assert_eq!(observations.len(), 3);
        for observation in observations {
            let muted = observation["microphone_mute"].as_bool().expect("hardware mute");
            config.input_mute = muted;
            let observed = Lights::observed(Some(&config), Some(ApiVersion::new(5, 4)));
            assert_eq!(observed.levels, Some([true, true, true, false, false, false, false]));
            assert_eq!(observed.ring(), Some(if muted { DANGER } else { WHITE }));
            assert_eq!(observation["ring"], if muted { "red" } else { "white" });
            assert_eq!(observation["positions"], serde_json::json!([1, 2, 3]));
            assert_eq!(lights(Some(&config), Some(ApiVersion::new(5, 4)), 360.).len(), 5);
            assert!(level_hint(Some(&config), Some(ApiVersion::new(5, 4))).is_none());
        }
    }

    #[test]
    fn led_flip_mirrors_the_level_leds_in_every_mode() {
        let mut config = config();
        config.input_gain.raw = 4096;
        config.headphone_volume.raw = -1152;
        config.direct_monitor.raw = 12800;
        for target in
            [VolumeSelection::Microphone, VolumeSelection::Headphone, VolumeSelection::Mix]
        {
            config.volume_select = target;
            config.leds_flip = false;
            let mut expected =
                level_pattern(&config, Some(ApiVersion::new(5, 4))).expect("measured pattern");
            expected.reverse();
            config.leds_flip = true;
            assert_eq!(level_pattern(&config, Some(ApiVersion::new(5, 4))), Some(expected));
        }
    }

    #[test]
    fn headphone_mute_darkens_only_the_headphone_level() {
        let mut config = config();
        config.headphone_volume.raw = -1152;
        config.direct_monitor.raw = 12800;
        config.input_gain.raw = 4096;
        for target in
            [VolumeSelection::Microphone, VolumeSelection::Headphone, VolumeSelection::Mix]
        {
            config.volume_select = target;
            config.headphone_mute = false;
            let live = Lights::observed(Some(&config), Some(ApiVersion::new(5, 4)));
            config.headphone_mute = true;
            let muted = Lights::observed(Some(&config), Some(ApiVersion::new(5, 4)));
            let expected =
                if target == VolumeSelection::Headphone { Some([false; 7]) } else { live.levels };
            assert_eq!(muted.levels, expected);
            assert_eq!(muted.ring(), Some(WHITE));
            assert!(level_hint(Some(&config), Some(ApiVersion::new(5, 4))).is_none());
        }
    }

    #[test]
    fn microphone_mute_changes_only_the_ring_in_every_mode() {
        let mut config = config();
        config.headphone_volume.raw = -1152;
        config.direct_monitor.raw = 12800;
        for target in
            [VolumeSelection::Microphone, VolumeSelection::Headphone, VolumeSelection::Mix]
        {
            config.volume_select = target;
            config.input_mute = false;
            let live = Lights::observed(Some(&config), Some(ApiVersion::new(5, 4)));
            config.input_mute = true;
            let muted = Lights::observed(Some(&config), Some(ApiVersion::new(5, 4)));
            assert!(live.levels.is_some());
            assert_eq!(muted.levels, live.levels);
            assert_eq!(muted.ring(), Some(DANGER));
            assert!(level_hint(Some(&config), Some(ApiVersion::new(5, 4))).is_none());
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
    fn the_glow_backdrop_matches_the_drawn_light_and_ends_at_the_rim() {
        let area = gpui::Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(200.), px(200.)));
        let background = 0x0007_080A;
        let plain = rgb(background);
        for glow in [Glow::Dark, Glow::Live, Glow::Muted] {
            let (color, strength) = glow.light();
            let center = glow.backdrop(area, gpui::point(px(100.), px(100.)), background);
            let expected = f32::from(u8::try_from(color >> 16).expect("red")) / 255. * strength
                + plain.r * (1. - strength);
            assert!((center.r - expected).abs() < 1e-4);
            // At and beyond the rim the backdrop is the plain background.
            for at in [gpui::point(px(200.), px(100.)), gpui::point(px(400.), px(400.))] {
                let rim = glow.backdrop(area, at, background);
                assert!((rim.r - plain.r).abs() < 1e-4 && (rim.b - plain.b).abs() < 1e-4);
            }
            // The light only fades outward.
            let near = glow.backdrop(area, gpui::point(px(130.), px(100.)), background);
            let far = glow.backdrop(area, gpui::point(px(170.), px(100.)), background);
            assert!(near.r >= far.r && far.r >= plain.r);
        }
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
            // This fixture has zero microphone gain, maximum headphone level,
            // and zero blend. Each mode includes exactly its own mode light.
            let expected_lights = match config.volume_select {
                VolumeSelection::Microphone => 2,
                VolumeSelection::Headphone => 9,
                VolumeSelection::Mix => 3,
            };
            assert_eq!(
                lights(Some(&config), Some(ApiVersion::new(5, 4)), 360.).len(),
                expected_lights
            );
        }
    }
}
