//! Control-side conversion and freshness for the existing bounded meter slot.
use super::{AudioSupervisor, Duration, Instant, MixerRuntimeState, SupervisedAudioHost};
use librewave_core::{MeterFrame, MeterSnapshot, SourceLevels, StereoLevels};
use librewave_engine::{BlockMeters, StereoMeter};

const READ_INTERVAL: Duration = Duration::from_millis(100);
const MAX_AGE: Duration = Duration::from_millis(350);

impl<H: SupervisedAudioHost> AudioSupervisor<H> {
    pub(crate) fn poll_meters(&mut self) {
        self.poll_meters_at(Instant::now());
    }

    pub(super) fn poll_meters_at(&mut self, now: Instant) {
        if self.runtime() != MixerRuntimeState::Active {
            self.meter_frame = None;
            return;
        }
        if now < self.next_meter_read {
            return;
        }
        self.next_meter_read = now + READ_INTERVAL;
        if let Some(block) = self.attempt.as_mut().and_then(|attempt| attempt.host.take_meters()) {
            self.meter_sequence = self.meter_sequence.saturating_add(1);
            self.meter_frame = Some((frame(&block, self.meter_sequence), now));
        }
    }

    pub(crate) fn meter_snapshot(&self) -> MeterSnapshot {
        self.meter_snapshot_at(Instant::now())
    }

    pub(super) fn meter_snapshot_at(&self, now: Instant) -> MeterSnapshot {
        match self.runtime() {
            MixerRuntimeState::Inactive { reason } => MeterSnapshot::Unavailable { reason },
            MixerRuntimeState::Active => match &self.meter_frame {
                Some((frame, observed)) if now.saturating_duration_since(*observed) <= MAX_AGE => {
                    MeterSnapshot::Live { frame: frame.clone() }
                }
                _ => MeterSnapshot::Waiting,
            },
        }
    }
}

fn frame(block: &BlockMeters, sequence: u64) -> MeterFrame {
    MeterFrame {
        sequence,
        sources: block
            .sources()
            .iter()
            .map(|source| SourceLevels { source: source.source(), levels: levels(source.meter()) })
            .collect(),
        outputs: std::array::from_fn(|index| {
            let endpoint = librewave_core::MIXER_OUTPUT_ENDPOINTS[index];
            levels(block.endpoint(endpoint).expect("engine meters cover every output"))
        }),
    }
}

fn levels(meter: StereoMeter) -> StereoLevels {
    StereoLevels {
        peak_left: amplitude(meter.left().peak()),
        peak_right: amplitude(meter.right().peak()),
        rms_left: amplitude(meter.left().rms()),
        rms_right: amplitude(meter.right().rms()),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn amplitude(value: f32) -> u32 {
    // Engine meters are finite and nonnegative. Saturation retains overload
    // information without putting floating-point values in the IPC schema.
    (f64::from(value) * 1_000_000.).round().min(f64::from(u32::MAX)) as u32
}
