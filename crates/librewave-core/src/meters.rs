//! Transient software meters. Amplitudes are linear millionths of full scale.
use crate::{MixerInactiveReason, SourceId};
use serde::{Deserialize, Serialize};

/// The bounded source count shared by profiles, meters, and the engine.
pub const MAX_MIXER_SOURCES: usize = 12;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StereoLevels {
    pub peak_left: u32,
    pub peak_right: u32,
    pub rms_left: u32,
    pub rms_right: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLevels {
    pub source: SourceId,
    pub levels: StereoLevels,
}

/// One processed-block observation. This data is never saved in a profile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "FrameWire")]
pub struct MeterFrame {
    pub sequence: u64,
    pub sources: Vec<SourceLevels>,
    /// Microphone, Monitor mix, and Stream mix, in output contract order.
    pub outputs: [StereoLevels; 3],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameWire {
    sequence: u64,
    sources: Vec<SourceLevels>,
    outputs: [StereoLevels; 3],
}

impl TryFrom<FrameWire> for MeterFrame {
    type Error = &'static str;
    fn try_from(wire: FrameWire) -> Result<Self, Self::Error> {
        if wire.sequence == 0 || wire.sources.is_empty() || wire.sources.len() > MAX_MIXER_SOURCES {
            return Err("invalid meter sequence or source count");
        }
        for (index, source) in wire.sources.iter().enumerate() {
            if source.source.get() == 0
                || wire.sources[..index].iter().any(|previous| previous.source == source.source)
            {
                return Err("invalid or duplicate meter source");
            }
        }
        Ok(Self { sequence: wire.sequence, sources: wire.sources, outputs: wire.outputs })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum MeterSnapshot {
    Unavailable {
        reason: MixerInactiveReason,
    },
    /// No recent processed block is available. This is not measured silence.
    Waiting,
    Live {
        frame: MeterFrame,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn meter_wire_rejects_unbounded_duplicate_and_invalid_sources() {
        let source = json!({"source":1,"levels":{"peak_left":0,"peak_right":0,
            "rms_left":0,"rms_right":0}});
        let outputs = vec![source["levels"].clone(); 3];
        let valid = json!({"sequence":1,"sources":[source.clone()],"outputs":outputs});
        assert!(serde_json::from_value::<MeterFrame>(valid.clone()).is_ok());
        for sources in [vec![], vec![source.clone(); 2], vec![source; MAX_MIXER_SOURCES + 1]] {
            let mut invalid = valid.clone();
            invalid["sources"] = json!(sources);
            assert!(serde_json::from_value::<MeterFrame>(invalid).is_err());
        }
        let mut invalid = valid;
        invalid["sequence"] = json!(0);
        assert!(serde_json::from_value::<MeterFrame>(invalid).is_err());
    }
}
