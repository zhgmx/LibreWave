//! Transient meter reads do not change control generations or pointer previews.
use super::{
    Shell,
    controls::tip,
    theme::{DANGER, FAINT, LINE},
};
use gpui::{Context, Div, SharedString, Stateful, div, prelude::*, px, rgb};
use librewave_core::{Command, MeterFrame, MeterSnapshot, MixerRuntimeState, StereoLevels};
use librewave_ipc::Response;
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};

impl Shell {
    fn audio_active(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.mixer.runtime == MixerRuntimeState::Active)
    }

    pub(super) fn poll_meters(&mut self, cx: &mut Context<Self>) {
        if self.expire_meters(std::time::Instant::now()) {
            cx.notify();
        }
        if self.pending || self.meter_request_epoch.is_some() || !self.audio_active() {
            return;
        }
        let epoch = self.request_epoch;
        self.meter_request_epoch = Some(epoch);
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        Client::new(configured_socket_path()).request(Command::GetMeters)
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                if this.apply_meter_result(epoch, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_meter_result(&mut self, epoch: u64, result: Result<Response, ClientError>) -> bool {
        self.meter_request_epoch = None;
        if epoch != self.request_epoch {
            return false;
        }
        let (next, error): (Option<MeterFrame>, Option<String>) = if self.audio_active() {
            match result {
                Ok(Response::Meters { meters: MeterSnapshot::Live { frame } }) => {
                    (Some(frame), None)
                }
                Ok(Response::Meters { meters: MeterSnapshot::Waiting }) => {
                    (None, Some("No recent audio block.".to_owned()))
                }
                Ok(Response::Meters { meters: MeterSnapshot::Unavailable { reason } }) => {
                    (None, Some(reason.to_string()))
                }
                Ok(_) => {
                    (None, Some("The daemon returned an unexpected meter response.".to_owned()))
                }
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        let changed = self.meters != next || self.meter_error != error;
        self.meter_error = error;
        self.meters_received = next.as_ref().map(|_| std::time::Instant::now());
        self.meters = next;
        changed
    }

    fn expire_meters(&mut self, now: std::time::Instant) -> bool {
        if self.meters_received.is_some_and(|received| {
            now.saturating_duration_since(received) > std::time::Duration::from_millis(350)
        }) {
            self.meters_received = None;
            self.meter_error = Some("Meter reading expired.".to_owned());
            return self.meters.take().is_some();
        }
        false
    }
}

/// Input and output peaks use the same -60 dBFS through 0 dBFS scale.
pub(super) fn bar(
    key: &str,
    levels: Option<StereoLevels>,
    color: u32,
    error: Option<&str>,
) -> Stateful<Div> {
    let mut view = div().w_full().flex().flex_col().gap(px(3.));
    for peak in [levels.map(|v| v.peak_left), levels.map(|v| v.peak_right)] {
        let track = div().w_full().h(px(3.)).rounded_full().bg(rgb(LINE));
        view = view.child(if let Some(peak) = peak {
            track.child(
                div()
                    .h_full()
                    .w(gpui::relative(fraction(peak)))
                    .rounded_full()
                    .bg(rgb(if peak > 1_000_000 { DANGER } else { color })),
            )
        } else {
            track
        });
    }
    view.id(SharedString::from(format!("meter-{key}")))
        .when(levels.is_none(), |v| v.opacity(0.4).text_color(rgb(FAINT)))
        .tooltip(tip((if levels.is_some() {
            "Stereo peak level, −60 to 0 dBFS"
        } else {
            error.unwrap_or("Meter unavailable")
        })
        .to_owned()))
}

#[allow(clippy::cast_possible_truncation)]
fn fraction(peak: u32) -> f32 {
    if peak == 0 {
        return 0.;
    }
    ((20. * (f64::from(peak) / 1_000_000.).log10() + 60.) / 60.).clamp(0., 1.) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_scale_distinguishes_silence_and_measured_signal() {
        assert!(fraction(0).abs() < f32::EPSILON);
        assert!((fraction(1_000_000) - 1.).abs() < f32::EPSILON);
        assert!((fraction(10_000) - 1. / 3.).abs() < 0.0001);
        assert!((fraction(u32::MAX) - 1.).abs() < f32::EPSILON);
    }

    #[test]
    fn late_meter_result_cannot_repopulate_a_stopped_session_or_change_fader_intent() {
        let mut snapshot = librewave_core::Snapshot::empty();
        snapshot.mixer = librewave_core::MixerSnapshot::active(snapshot.mixer.profile.clone());
        let mut shell = super::super::tests::shell_with(snapshot.clone());
        let frame = MeterFrame {
            sequence: 1,
            sources: vec![librewave_core::SourceLevels {
                source: librewave_core::MICROPHONE_SOURCE_ID,
                levels: StereoLevels::default(),
            }],
            outputs: [StereoLevels::default(); 3],
        };
        let response = Response::Meters { meters: MeterSnapshot::Live { frame } };
        let edit = super::super::mixer::FaderEdit {
            source: librewave_core::SYSTEM_SOURCE_ID,
            target: librewave_core::MixTarget::Stream,
            fader: librewave_core::FaderGain::MIN,
        };
        shell.queued_fader = Some(edit);
        shell.request_epoch = 1;
        assert!(!shell.apply_meter_result(0, Ok(response.clone())));
        assert!(shell.meters.is_none());
        assert!(shell.apply_meter_result(1, Ok(response.clone())));
        assert_eq!(shell.snapshot, Some(snapshot));
        assert_eq!(shell.queued_fader, Some(edit));
        let received = shell.meters_received.expect("meter receipt");
        assert!(!shell.expire_meters(received + std::time::Duration::from_millis(350)));
        assert!(shell.expire_meters(received + std::time::Duration::from_millis(351)));
        assert!(shell.meters.is_none());
        assert!(shell.apply_meter_result(1, Ok(response.clone())));
        shell.snapshot = Some(librewave_core::Snapshot::empty());
        assert!(shell.apply_meter_result(1, Ok(response)));
        assert!(shell.meters.is_none());
    }

    #[test]
    fn unexpected_meter_responses_clear_values_and_keep_an_explicit_error() {
        let mut snapshot = librewave_core::Snapshot::empty();
        snapshot.mixer = librewave_core::MixerSnapshot::active(snapshot.mixer.profile.clone());
        let mut shell = super::super::tests::shell_with(snapshot.clone());
        assert!(shell.apply_meter_result(0, Ok(Response::Status { snapshot })));
        assert!(shell.meters.is_none());
        assert_eq!(
            shell.meter_error.as_deref(),
            Some("The daemon returned an unexpected meter response.")
        );
    }
}
