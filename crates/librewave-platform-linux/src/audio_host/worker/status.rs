use super::super::clock_bridge::{CaptureClockObservation, PlaybackClockObservation};
use super::super::pcm::PhysicalPcmParameters;
use librewave_engine::{
    ClockAttemptEpoch, ClockFramePosition, ClockObservation, MonotonicNanoseconds,
};

const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) struct PcmStatusSnapshot {
    pub timestamp_seconds: i64,
    pub timestamp_nanoseconds: i64,
    pub available_frames: i64,
    pub delay_frames: i64,
    pub state_raw: i32,
    pub geometry: PhysicalPcmParameters,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) enum PcmStatusError {
    NegativeTimestampSeconds { actual: i64 },
    InvalidTimestampNanoseconds { actual: i64 },
    TimestampOverflow,
    Geometry { expected: PhysicalPcmParameters, actual: PhysicalPcmParameters },
    NegativeAvailability { actual: i64 },
    AvailabilityOutsideBuffer { actual: u64, buffer_frames: u64 },
    PositionOverflow,
    PlaybackPositionBeforeQueue { application: u64, queued: u64 },
    Xrun,
    Suspended,
    Disconnected,
    InvalidState { actual: i32, expected: PcmExpectedState },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) enum PcmExpectedState {
    CaptureRunning,
    PlaybackPrepared,
    PlaybackRunning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) struct ValidatedCaptureStatus {
    pub observation: CaptureClockObservation,
    pub available_frames: u64,
    pub delay_frames: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::audio_host) struct ValidatedPlaybackStatus {
    pub observation: PlaybackClockObservation,
    pub available_frames: u64,
    pub queued_frames: u64,
    pub delay_frames: i64,
}

pub(in crate::audio_host) fn validate_capture_status(
    epoch: ClockAttemptEpoch,
    application_frames: u64,
    expected_geometry: PhysicalPcmParameters,
    snapshot: PcmStatusSnapshot,
) -> Result<ValidatedCaptureStatus, PcmStatusError> {
    validate_geometry(expected_geometry, snapshot.geometry)?;
    validate_state(snapshot.state_raw, PcmExpectedState::CaptureRunning)?;
    let monotonic_time = monotonic_nanoseconds(snapshot)?;
    let available_frames = availability(snapshot, expected_geometry)?;
    let hardware_frames =
        application_frames.checked_add(available_frames).ok_or(PcmStatusError::PositionOverflow)?;
    Ok(ValidatedCaptureStatus {
        observation: CaptureClockObservation::new(ClockObservation::new(
            epoch,
            ClockFramePosition::new(hardware_frames),
            MonotonicNanoseconds::new(monotonic_time),
        )),
        available_frames,
        delay_frames: snapshot.delay_frames,
    })
}

pub(in crate::audio_host) fn validate_playback_status(
    epoch: ClockAttemptEpoch,
    application_frames: u64,
    expected_geometry: PhysicalPcmParameters,
    pcm_started: bool,
    snapshot: PcmStatusSnapshot,
) -> Result<ValidatedPlaybackStatus, PcmStatusError> {
    validate_geometry(expected_geometry, snapshot.geometry)?;
    let expected_state = if pcm_started {
        PcmExpectedState::PlaybackRunning
    } else {
        PcmExpectedState::PlaybackPrepared
    };
    validate_state(snapshot.state_raw, expected_state)?;
    let monotonic_time = monotonic_nanoseconds(snapshot)?;
    let available_frames = availability(snapshot, expected_geometry)?;
    let buffer_frames = u64::from(expected_geometry.buffer_frames);
    let queued_frames = buffer_frames - available_frames;
    let hardware_frames = application_frames.checked_sub(queued_frames).ok_or(
        PcmStatusError::PlaybackPositionBeforeQueue {
            application: application_frames,
            queued: queued_frames,
        },
    )?;
    Ok(ValidatedPlaybackStatus {
        observation: PlaybackClockObservation::new(
            ClockObservation::new(
                epoch,
                ClockFramePosition::new(hardware_frames),
                MonotonicNanoseconds::new(monotonic_time),
            ),
            ClockFramePosition::new(application_frames),
        ),
        available_frames,
        queued_frames,
        delay_frames: snapshot.delay_frames,
    })
}

fn validate_geometry(
    expected: PhysicalPcmParameters,
    actual: PhysicalPcmParameters,
) -> Result<(), PcmStatusError> {
    if expected == actual { Ok(()) } else { Err(PcmStatusError::Geometry { expected, actual }) }
}

fn monotonic_nanoseconds(snapshot: PcmStatusSnapshot) -> Result<u64, PcmStatusError> {
    let seconds = u64::try_from(snapshot.timestamp_seconds).map_err(|_| {
        PcmStatusError::NegativeTimestampSeconds { actual: snapshot.timestamp_seconds }
    })?;
    let nanoseconds = u64::try_from(snapshot.timestamp_nanoseconds).map_err(|_| {
        PcmStatusError::InvalidTimestampNanoseconds { actual: snapshot.timestamp_nanoseconds }
    })?;
    if nanoseconds >= NANOSECONDS_PER_SECOND {
        return Err(PcmStatusError::InvalidTimestampNanoseconds {
            actual: snapshot.timestamp_nanoseconds,
        });
    }
    seconds
        .checked_mul(NANOSECONDS_PER_SECOND)
        .and_then(|value| value.checked_add(nanoseconds))
        .ok_or(PcmStatusError::TimestampOverflow)
}

fn availability(
    snapshot: PcmStatusSnapshot,
    geometry: PhysicalPcmParameters,
) -> Result<u64, PcmStatusError> {
    let available_frames = u64::try_from(snapshot.available_frames)
        .map_err(|_| PcmStatusError::NegativeAvailability { actual: snapshot.available_frames })?;
    let buffer_frames = u64::from(geometry.buffer_frames);
    if available_frames > buffer_frames {
        return Err(PcmStatusError::AvailabilityOutsideBuffer {
            actual: available_frames,
            buffer_frames,
        });
    }
    Ok(available_frames)
}

fn validate_state(raw: i32, expected: PcmExpectedState) -> Result<(), PcmStatusError> {
    use alsa::pcm::State;

    if raw == State::XRun as i32 {
        return Err(PcmStatusError::Xrun);
    }
    if raw == State::Suspended as i32 {
        return Err(PcmStatusError::Suspended);
    }
    if raw == State::Disconnected as i32 {
        return Err(PcmStatusError::Disconnected);
    }
    let wanted = match expected {
        PcmExpectedState::CaptureRunning | PcmExpectedState::PlaybackRunning => {
            State::Running as i32
        }
        PcmExpectedState::PlaybackPrepared => State::Prepared as i32,
    };
    if raw == wanted { Ok(()) } else { Err(PcmStatusError::InvalidState { actual: raw, expected }) }
}
