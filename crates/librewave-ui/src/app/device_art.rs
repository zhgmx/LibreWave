//! Presentation of observed hardware lights. This module never reads hardware.
use gpui::{Image, ImageFormat};
use librewave_core::{VolumeSelection, Wave3ConfigSnapshot};
use std::sync::Arc;

const SVG: &str = include_str!("../../assets/devices/wave3.svg");
const OFF: &str = "#10141a";
const WHITE: &str = "#f5f7ff";
const RED: &str = "#ff394b";
const RING_OFF: &str = "#505864";

#[derive(Default)]
struct Lights {
    mode: [bool; 3],
    // No verified firmware thresholds or LED mask are available in IPC yet.
    // Keep these off instead of inventing a hardware level indication.
    levels: [bool; 7],
    mute: Option<bool>,
}

impl Lights {
    fn observed(config: Option<&Wave3ConfigSnapshot>) -> Self {
        let Some(config) = config.filter(|config| !config.all_leds_off) else {
            return Self::default();
        };
        let mut mode = [false; 3];
        mode[match config.volume_select {
            VolumeSelection::Microphone => 0,
            VolumeSelection::Headphone => 1,
            VolumeSelection::Mix => 2,
        }] = true;
        Self { mode, mute: Some(config.input_mute), ..Self::default() }
    }

    fn svg(&self) -> String {
        let mut svg = SVG.to_owned();
        for (prefix, lights) in [("mode", self.mode.as_slice()), ("level", self.levels.as_slice())]
        {
            for (index, lit) in lights.iter().enumerate() {
                paint(&mut svg, &format!("{prefix}-{index}"), OFF, if *lit { WHITE } else { OFF });
            }
        }
        paint(
            &mut svg,
            "mute-ring",
            RING_OFF,
            match self.mute {
                Some(true) => RED,
                Some(false) => WHITE,
                None => RING_OFF,
            },
        );
        svg
    }
}

fn paint(svg: &mut String, id: &str, original: &str, color: &str) {
    *svg = svg.replace(
        &format!("id=\"{id}\"><stop stop-color=\"{original}\""),
        &format!("id=\"{id}\"><stop stop-color=\"{color}\""),
    );
}

pub(super) fn image(config: Option<&Wave3ConfigSnapshot>) -> Arc<Image> {
    // GPUI keys encoded images by their content hash, so identical light states
    // share the decoded image without a second application state cache.
    Arc::new(Image::from_bytes(ImageFormat::Svg, Lights::observed(config).svg().into_bytes()))
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

    fn has_paint(svg: &str, id: &str, color: &str) -> bool {
        svg.contains(&format!("id=\"{id}\"><stop stop-color=\"{color}\""))
    }

    #[test]
    fn ring_tracks_hardware_microphone_mute_only() {
        let mut config = config();
        config.headphone_mute = true;
        assert!(has_paint(&Lights::observed(Some(&config)).svg(), "mute-ring", WHITE));
        config.input_mute = true;
        assert!(has_paint(&Lights::observed(Some(&config)).svg(), "mute-ring", RED));
    }

    #[test]
    fn unavailable_state_and_leds_off_do_not_invent_lit_indicators() {
        let mut config = config();
        config.input_mute = true;
        config.all_leds_off = true;
        for lights in [Lights::observed(None), Lights::observed(Some(&config))] {
            assert_eq!(lights.mode, [false; 3]);
            assert_eq!(lights.levels, [false; 7]);
            assert!(has_paint(&lights.svg(), "mute-ring", RING_OFF));
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
            let lights = Lights::observed(Some(&config));
            for position in 0..3 {
                assert_eq!(lights.mode[position], position == index);
                assert!(has_paint(
                    &lights.svg(),
                    &format!("mode-{position}"),
                    if position == index { WHITE } else { OFF },
                ));
            }
            assert_eq!(lights.levels, [false; 7]);
        }
    }

    #[test]
    fn all_seven_level_dots_can_be_painted_independently() {
        for index in 0..7 {
            let mut lights = Lights::default();
            lights.levels[index] = true;
            let svg = lights.svg();
            for position in 0..7 {
                assert!(has_paint(
                    &svg,
                    &format!("level-{position}"),
                    if position == index { WHITE } else { OFF },
                ));
            }
        }
    }
}
