//! Portable relative clock-rate estimation from checked frame/time observations.

use crate::RateMatchRatioBounds;
use std::fmt;
use std::num::NonZeroU64;

/// A nonzero identity for one attempt whose clock history is continuous.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClockAttemptEpoch(NonZeroU64);

impl ClockAttemptEpoch {
    /// Constructs a nonzero attempt epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is zero.
    pub const fn try_new(value: u64) -> Result<Self, ClockAttemptEpochError> {
        match NonZeroU64::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(ClockAttemptEpochError::Zero),
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Why an attempt epoch was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockAttemptEpochError {
    Zero,
}

impl fmt::Display for ClockAttemptEpochError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("clock attempt epoch must be nonzero")
    }
}

impl std::error::Error for ClockAttemptEpochError {}

/// A checked integer frame position in one clock domain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClockFramePosition(u64);

impl ClockFramePosition {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn checked_add(self, frames: u64) -> Option<Self> {
        match self.0.checked_add(frames) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// Nanoseconds from an externally observed monotonic clock.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonotonicNanoseconds(u64);

impl MonotonicNanoseconds {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One frame position and timestamp from one continuous attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockObservation {
    epoch: ClockAttemptEpoch,
    frame_position: ClockFramePosition,
    monotonic_time: MonotonicNanoseconds,
}

impl ClockObservation {
    #[must_use]
    pub const fn new(
        epoch: ClockAttemptEpoch,
        frame_position: ClockFramePosition,
        monotonic_time: MonotonicNanoseconds,
    ) -> Self {
        Self { epoch, frame_position, monotonic_time }
    }

    #[must_use]
    pub const fn epoch(self) -> ClockAttemptEpoch {
        self.epoch
    }

    #[must_use]
    pub const fn frame_position(self) -> ClockFramePosition {
        self.frame_position
    }

    #[must_use]
    pub const fn monotonic_time(self) -> MonotonicNanoseconds {
        self.monotonic_time
    }
}

/// Caller-selected positive evidence bounds for one clock domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockDeltaBounds {
    minimum_frames: u64,
    maximum_frames: u64,
    minimum_nanoseconds: u64,
    maximum_nanoseconds: u64,
}

impl ClockDeltaBounds {
    /// Constructs frame/time evidence bounds without assuming a fixed quantum.
    ///
    /// Every positive delta up to the configured maximum is accepted. A delta
    /// below either minimum is valid priming evidence.
    ///
    /// # Errors
    ///
    /// Returns an error for zero or unordered bounds.
    pub const fn try_new(
        minimum_frames: u64,
        maximum_frames: u64,
        minimum_nanoseconds: u64,
        maximum_nanoseconds: u64,
    ) -> Result<Self, ClockRateEstimatorConfigError> {
        if minimum_frames == 0 || maximum_frames < minimum_frames {
            return Err(ClockRateEstimatorConfigError::InvalidFrameDeltaBounds {
                minimum: minimum_frames,
                maximum: maximum_frames,
            });
        }
        if minimum_nanoseconds == 0 || maximum_nanoseconds < minimum_nanoseconds {
            return Err(ClockRateEstimatorConfigError::InvalidTimeDeltaBounds {
                minimum: minimum_nanoseconds,
                maximum: maximum_nanoseconds,
            });
        }
        Ok(Self { minimum_frames, maximum_frames, minimum_nanoseconds, maximum_nanoseconds })
    }

    #[must_use]
    pub const fn minimum_frames(self) -> u64 {
        self.minimum_frames
    }

    #[must_use]
    pub const fn maximum_frames(self) -> u64 {
        self.maximum_frames
    }

    #[must_use]
    pub const fn minimum_nanoseconds(self) -> u64 {
        self.minimum_nanoseconds
    }

    #[must_use]
    pub const fn maximum_nanoseconds(self) -> u64 {
        self.maximum_nanoseconds
    }
}

/// Caller-selected policy for one output-rate/input-rate estimator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockRateEstimatorConfig {
    input: ClockDeltaBounds,
    output: ClockDeltaBounds,
    maximum_observation_skew_nanoseconds: u64,
    ratio_bounds: RateMatchRatioBounds,
}

impl ClockRateEstimatorConfig {
    #[must_use]
    pub const fn new(
        input: ClockDeltaBounds,
        output: ClockDeltaBounds,
        maximum_observation_skew_nanoseconds: u64,
        ratio_bounds: RateMatchRatioBounds,
    ) -> Self {
        Self { input, output, maximum_observation_skew_nanoseconds, ratio_bounds }
    }

    #[must_use]
    pub const fn input(self) -> ClockDeltaBounds {
        self.input
    }

    #[must_use]
    pub const fn output(self) -> ClockDeltaBounds {
        self.output
    }

    #[must_use]
    pub const fn maximum_observation_skew_nanoseconds(self) -> u64 {
        self.maximum_observation_skew_nanoseconds
    }

    #[must_use]
    pub const fn ratio_bounds(self) -> RateMatchRatioBounds {
        self.ratio_bounds
    }
}

/// Why estimator policy could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockRateEstimatorConfigError {
    InvalidFrameDeltaBounds { minimum: u64, maximum: u64 },
    InvalidTimeDeltaBounds { minimum: u64, maximum: u64 },
}

impl fmt::Display for ClockRateEstimatorConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrameDeltaBounds { minimum, maximum } => write!(
                formatter,
                "clock frame delta bounds must be positive and ordered: {minimum}..={maximum}"
            ),
            Self::InvalidTimeDeltaBounds { minimum, maximum } => write!(
                formatter,
                "clock time delta bounds must be positive and ordered: {minimum}..={maximum}"
            ),
        }
    }
}

impl std::error::Error for ClockRateEstimatorConfigError {}

/// Input or output side of a relative-rate estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockRateSide {
    Input,
    Output,
}

/// The candidate estimator state after one accepted observation pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockRateEstimate {
    Priming,
    Measured { ratio: f64, updated: bool },
}

impl ClockRateEstimate {
    #[must_use]
    pub const fn ratio(self) -> Option<f64> {
        match self {
            Self::Priming => None,
            Self::Measured { ratio, .. } => Some(ratio),
        }
    }
}

/// Why an observation pair could not extend one continuous rate history.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockRateEstimatorError {
    CrossClockEpoch {
        input: ClockAttemptEpoch,
        output: ClockAttemptEpoch,
    },
    AttemptEpochChanged {
        expected: ClockAttemptEpoch,
        actual: ClockAttemptEpoch,
    },
    RepeatedFramePosition {
        side: ClockRateSide,
        position: ClockFramePosition,
    },
    FramePositionRegression {
        side: ClockRateSide,
        previous: ClockFramePosition,
        actual: ClockFramePosition,
    },
    RepeatedMonotonicTime {
        side: ClockRateSide,
        time: MonotonicNanoseconds,
    },
    MonotonicTimeRegression {
        side: ClockRateSide,
        previous: MonotonicNanoseconds,
        actual: MonotonicNanoseconds,
    },
    FrameDeltaOutsideBounds {
        side: ClockRateSide,
        actual: u64,
        maximum: u64,
    },
    TimeDeltaOutsideBounds {
        side: ClockRateSide,
        actual: u64,
        maximum: u64,
    },
    ObservationSkew {
        actual_nanoseconds: u64,
        maximum_nanoseconds: u64,
    },
    CrossProductOverflow,
    InvalidRatio {
        actual: f64,
    },
    RatioOutsideBounds {
        actual: f64,
        minimum: f64,
        maximum: f64,
    },
}

impl fmt::Display for ClockRateEstimatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ClockRateEstimatorError {}

#[derive(Clone, Copy, Debug)]
struct ObservationPair {
    input: ClockObservation,
    output: ClockObservation,
}

/// Transactional estimator for `output_rate / input_rate`.
#[derive(Clone, Debug)]
pub struct ClockRateEstimator {
    config: ClockRateEstimatorConfig,
    epoch: Option<ClockAttemptEpoch>,
    anchor: Option<ObservationPair>,
    latest: Option<ObservationPair>,
    ratio: Option<f64>,
}

impl ClockRateEstimator {
    #[must_use]
    pub const fn new(config: ClockRateEstimatorConfig) -> Self {
        Self { config, epoch: None, anchor: None, latest: None, ratio: None }
    }

    #[must_use]
    pub const fn config(&self) -> ClockRateEstimatorConfig {
        self.config
    }

    /// Previews one observation pair without changing committed history.
    ///
    /// The estimator calculates `output_rate / input_rate`. A positive delta
    /// below a configured minimum is committed priming evidence. Fixed block
    /// size and scheduling policy remain outside this portable estimator.
    ///
    /// # Errors
    ///
    /// Returns an error for an epoch mismatch, repeated or regressing values,
    /// excessive delta/skew, checked-product failure, non-finite ratio, or a
    /// ratio outside caller policy. History remains unchanged.
    pub fn preview(
        &mut self,
        input: ClockObservation,
        output: ClockObservation,
    ) -> Result<ClockRateEstimatorStep<'_>, ClockRateEstimatorError> {
        if input.epoch != output.epoch {
            return Err(ClockRateEstimatorError::CrossClockEpoch {
                input: input.epoch,
                output: output.epoch,
            });
        }
        if let Some(expected) = self.epoch
            && expected != input.epoch
        {
            return Err(ClockRateEstimatorError::AttemptEpochChanged {
                expected,
                actual: input.epoch,
            });
        }
        validate_skew(self.config, input, output)?;
        if let Some(latest) = self.latest {
            validate_progress(ClockRateSide::Input, latest.input, input)?;
            validate_progress(ClockRateSide::Output, latest.output, output)?;
        }

        let pair = ObservationPair { input, output };
        let Some(anchor) = self.anchor else {
            return Ok(ClockRateEstimatorStep {
                estimator: self,
                epoch: input.epoch,
                anchor: pair,
                latest: pair,
                ratio: None,
                estimate: ClockRateEstimate::Priming,
            });
        };
        let input_delta =
            checked_delta(ClockRateSide::Input, anchor.input, input, self.config.input)?;
        let output_delta =
            checked_delta(ClockRateSide::Output, anchor.output, output, self.config.output)?;
        let enough_evidence = input_delta.frames >= self.config.input.minimum_frames
            && input_delta.nanoseconds >= self.config.input.minimum_nanoseconds
            && output_delta.frames >= self.config.output.minimum_frames
            && output_delta.nanoseconds >= self.config.output.minimum_nanoseconds;
        if !enough_evidence {
            let ratio = self.ratio;
            let estimate = ratio.map_or(ClockRateEstimate::Priming, |ratio| {
                ClockRateEstimate::Measured { ratio, updated: false }
            });
            return Ok(ClockRateEstimatorStep {
                estimator: self,
                epoch: input.epoch,
                anchor,
                latest: pair,
                ratio,
                estimate,
            });
        }

        let numerator = u128::from(output_delta.frames)
            .checked_mul(u128::from(input_delta.nanoseconds))
            .ok_or(ClockRateEstimatorError::CrossProductOverflow)?;
        let denominator = u128::from(input_delta.frames)
            .checked_mul(u128::from(output_delta.nanoseconds))
            .ok_or(ClockRateEstimatorError::CrossProductOverflow)?;
        #[allow(clippy::cast_precision_loss)]
        let ratio = numerator as f64 / denominator as f64;
        if !ratio.is_finite() || ratio <= 0.0 {
            return Err(ClockRateEstimatorError::InvalidRatio { actual: ratio });
        }
        let bounds = self.config.ratio_bounds;
        if ratio < bounds.minimum() || ratio > bounds.maximum() {
            return Err(ClockRateEstimatorError::RatioOutsideBounds {
                actual: ratio,
                minimum: bounds.minimum(),
                maximum: bounds.maximum(),
            });
        }
        Ok(ClockRateEstimatorStep {
            estimator: self,
            epoch: input.epoch,
            anchor: pair,
            latest: pair,
            ratio: Some(ratio),
            estimate: ClockRateEstimate::Measured { ratio, updated: true },
        })
    }

    /// Clears all observation and ratio history.
    pub fn reset(&mut self) {
        self.epoch = None;
        self.anchor = None;
        self.latest = None;
        self.ratio = None;
    }

    #[must_use]
    pub const fn ratio(&self) -> Option<f64> {
        self.ratio
    }

    #[must_use]
    pub const fn latest_input(&self) -> Option<ClockObservation> {
        match self.latest {
            Some(pair) => Some(pair.input),
            None => None,
        }
    }

    #[must_use]
    pub const fn latest_output(&self) -> Option<ClockObservation> {
        match self.latest {
            Some(pair) => Some(pair.output),
            None => None,
        }
    }
}

/// A borrow-scoped estimator candidate that can be committed once.
pub struct ClockRateEstimatorStep<'a> {
    estimator: &'a mut ClockRateEstimator,
    epoch: ClockAttemptEpoch,
    anchor: ObservationPair,
    latest: ObservationPair,
    ratio: Option<f64>,
    estimate: ClockRateEstimate,
}

impl ClockRateEstimatorStep<'_> {
    #[must_use]
    pub const fn estimate(&self) -> ClockRateEstimate {
        self.estimate
    }

    #[must_use]
    pub const fn ratio(&self) -> Option<f64> {
        self.estimate.ratio()
    }

    /// Commits this candidate to the exact estimator it borrowed.
    pub fn commit(self) {
        self.estimator.epoch = Some(self.epoch);
        self.estimator.anchor = Some(self.anchor);
        self.estimator.latest = Some(self.latest);
        self.estimator.ratio = self.ratio;
    }
}

impl fmt::Debug for ClockRateEstimatorStep<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClockRateEstimatorStep")
            .field("epoch", &self.epoch)
            .field("anchor", &self.anchor)
            .field("latest", &self.latest)
            .field("ratio", &self.ratio)
            .field("estimate", &self.estimate)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
struct ClockDelta {
    frames: u64,
    nanoseconds: u64,
}

fn validate_skew(
    config: ClockRateEstimatorConfig,
    input: ClockObservation,
    output: ClockObservation,
) -> Result<(), ClockRateEstimatorError> {
    let actual = input.monotonic_time.0.abs_diff(output.monotonic_time.0);
    if actual > config.maximum_observation_skew_nanoseconds {
        return Err(ClockRateEstimatorError::ObservationSkew {
            actual_nanoseconds: actual,
            maximum_nanoseconds: config.maximum_observation_skew_nanoseconds,
        });
    }
    Ok(())
}

fn validate_progress(
    side: ClockRateSide,
    previous: ClockObservation,
    actual: ClockObservation,
) -> Result<(), ClockRateEstimatorError> {
    if actual.frame_position == previous.frame_position {
        return Err(ClockRateEstimatorError::RepeatedFramePosition {
            side,
            position: actual.frame_position,
        });
    }
    if actual.frame_position < previous.frame_position {
        return Err(ClockRateEstimatorError::FramePositionRegression {
            side,
            previous: previous.frame_position,
            actual: actual.frame_position,
        });
    }
    if actual.monotonic_time == previous.monotonic_time {
        return Err(ClockRateEstimatorError::RepeatedMonotonicTime {
            side,
            time: actual.monotonic_time,
        });
    }
    if actual.monotonic_time < previous.monotonic_time {
        return Err(ClockRateEstimatorError::MonotonicTimeRegression {
            side,
            previous: previous.monotonic_time,
            actual: actual.monotonic_time,
        });
    }
    Ok(())
}

fn checked_delta(
    side: ClockRateSide,
    previous: ClockObservation,
    actual: ClockObservation,
    bounds: ClockDeltaBounds,
) -> Result<ClockDelta, ClockRateEstimatorError> {
    validate_progress(side, previous, actual)?;
    let frames = actual.frame_position.0 - previous.frame_position.0;
    if frames > bounds.maximum_frames {
        return Err(ClockRateEstimatorError::FrameDeltaOutsideBounds {
            side,
            actual: frames,
            maximum: bounds.maximum_frames,
        });
    }
    let nanoseconds = actual.monotonic_time.0 - previous.monotonic_time.0;
    if nanoseconds > bounds.maximum_nanoseconds {
        return Err(ClockRateEstimatorError::TimeDeltaOutsideBounds {
            side,
            actual: nanoseconds,
            maximum: bounds.maximum_nanoseconds,
        });
    }
    Ok(ClockDelta { frames, nanoseconds })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;

    fn epoch(value: u64) -> ClockAttemptEpoch {
        ClockAttemptEpoch::try_new(value).expect("nonzero test epoch")
    }

    fn observation(epoch: u64, frames: u64, nanoseconds: u64) -> ClockObservation {
        ClockObservation::new(
            self::epoch(epoch),
            ClockFramePosition::new(frames),
            MonotonicNanoseconds::new(nanoseconds),
        )
    }

    fn config(minimum_delta: u64, maximum_delta: u64) -> ClockRateEstimatorConfig {
        let deltas = ClockDeltaBounds::try_new(
            minimum_delta,
            maximum_delta,
            minimum_delta,
            maximum_delta.checked_mul(NANOSECONDS_PER_SECOND).expect("test time bound"),
        )
        .expect("test delta bounds");
        let ratios = RateMatchRatioBounds::try_new(2.0, 0.5, 2.0).expect("test ratio bounds");
        ClockRateEstimatorConfig::new(deltas, deltas, NANOSECONDS_PER_SECOND, ratios)
    }

    #[test]
    fn typed_clock_values_keep_integer_epoch_position_and_time() {
        assert_eq!(ClockAttemptEpoch::try_new(0), Err(ClockAttemptEpochError::Zero));
        let epoch = epoch(7);
        let position = ClockFramePosition::new(u64::MAX - 1);
        let time = MonotonicNanoseconds::new(123);
        let observation = ClockObservation::new(epoch, position, time);
        assert_eq!(observation.epoch(), epoch);
        assert_eq!(observation.frame_position(), position);
        assert_eq!(observation.monotonic_time(), time);
        assert_eq!(position.checked_add(1), Some(ClockFramePosition::new(u64::MAX)));
        assert_eq!(position.checked_add(2), None);
    }

    #[test]
    fn arbitrary_positive_deltas_are_committed_as_priming_until_the_required_span() {
        let mut estimator = ClockRateEstimator::new(config(100, 1_000));
        let first = estimator
            .preview(observation(1, 10, 100), observation(1, 20, 110))
            .expect("initial observation pair");
        assert_eq!(first.estimate(), ClockRateEstimate::Priming);
        first.commit();

        let small = estimator
            .preview(observation(1, 17, 213), observation(1, 29, 223))
            .expect("arbitrary positive priming delta");
        assert_eq!(small.estimate(), ClockRateEstimate::Priming);
        small.commit();
        assert_eq!(estimator.ratio(), None);

        let measured = estimator
            .preview(
                observation(1, 110, NANOSECONDS_PER_SECOND + 100),
                observation(1, 120, NANOSECONDS_PER_SECOND + 110),
            )
            .expect("complete evidence span");
        assert_eq!(measured.ratio(), Some(1.0));
        measured.commit();
        assert_eq!(estimator.ratio(), Some(1.0));
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn capture_and_playback_orientations_use_the_required_cross_products() {
        let mut capture = ClockRateEstimator::new(config(1, 100_000));
        capture
            .preview(observation(1, 0, 0), observation(1, 0, 0))
            .expect("capture anchor")
            .commit();
        let capture_ratio = capture
            .preview(
                observation(1, 48_048, 999_000_000),
                observation(1, 48_000, NANOSECONDS_PER_SECOND),
            )
            .expect("capture estimate")
            .ratio()
            .expect("capture measured ratio");
        let expected_capture = (48_000_u128 * 999_000_000_u128) as f64
            / (48_048_u128 * u128::from(NANOSECONDS_PER_SECOND)) as f64;
        assert!((capture_ratio - expected_capture).abs() < f64::EPSILON);

        let mut playback = ClockRateEstimator::new(config(1, 100_000));
        playback
            .preview(observation(2, 0, 0), observation(2, 0, 0))
            .expect("playback anchor")
            .commit();
        let playback_ratio = playback
            .preview(
                observation(2, 48_000, NANOSECONDS_PER_SECOND),
                observation(2, 47_952, 1_001_000_000),
            )
            .expect("playback estimate")
            .ratio()
            .expect("playback measured ratio");
        let expected_playback = (47_952_u128 * u128::from(NANOSECONDS_PER_SECOND)) as f64
            / (48_000_u128 * 1_001_000_000_u128) as f64;
        assert!((playback_ratio - expected_playback).abs() < f64::EPSILON);
    }

    #[test]
    fn absolute_monotonic_timestamp_offsets_do_not_change_the_measured_ratio() {
        fn measured_ratio(offset: u64) -> f64 {
            let mut estimator = ClockRateEstimator::new(config(1, 100_000));
            estimator
                .preview(observation(1, 0, offset + 10_000), observation(1, 0, offset + 10_020))
                .expect("offset anchor within skew policy")
                .commit();
            estimator
                .preview(observation(1, 100, offset + 11_000), observation(1, 99, offset + 11_120))
                .expect("offset estimate within skew policy")
                .ratio()
                .expect("measured ratio")
        }

        assert_eq!(measured_ratio(0), measured_ratio(4_000_000_000_000));
    }

    #[test]
    fn estimator_preview_is_transactional_and_rebases_only_after_commit() {
        let mut estimator = ClockRateEstimator::new(config(1, 100_000));
        estimator.preview(observation(1, 0, 0), observation(1, 0, 0)).expect("anchor").commit();
        {
            let step = estimator
                .preview(
                    observation(1, 48_000, NANOSECONDS_PER_SECOND),
                    observation(1, 48_000, NANOSECONDS_PER_SECOND),
                )
                .expect("uncommitted estimate");
            assert_eq!(step.ratio(), Some(1.0));
        }
        assert_eq!(estimator.ratio(), None);
        assert_eq!(estimator.latest_input(), Some(observation(1, 0, 0)));

        estimator
            .preview(
                observation(1, 48_000, NANOSECONDS_PER_SECOND),
                observation(1, 48_000, NANOSECONDS_PER_SECOND),
            )
            .expect("committed estimate")
            .commit();
        assert_eq!(estimator.ratio(), Some(1.0));

        let holding = estimator
            .preview(
                observation(1, 48_001, NANOSECONDS_PER_SECOND + 1),
                observation(1, 48_001, NANOSECONDS_PER_SECOND + 1),
            )
            .expect("positive evidence after rebase");
        assert_eq!(holding.estimate(), ClockRateEstimate::Measured { ratio: 1.0, updated: true });
    }

    #[test]
    fn invalid_observations_and_policy_violations_do_not_mutate_history() {
        let mut estimator = ClockRateEstimator::new(config(1, 100));
        estimator
            .preview(observation(1, 10, 100), observation(1, 20, 100))
            .expect("anchor")
            .commit();
        let initial = (estimator.ratio(), estimator.latest_input(), estimator.latest_output());

        for error in [
            estimator
                .preview(observation(2, 11, 101), observation(2, 21, 101))
                .expect_err("attempt epoch change"),
            estimator
                .preview(observation(1, 10, 101), observation(1, 21, 101))
                .expect_err("repeated frame"),
            estimator
                .preview(observation(1, 9, 101), observation(1, 21, 101))
                .expect_err("regressing frame"),
            estimator
                .preview(observation(1, 11, 100), observation(1, 21, 101))
                .expect_err("repeated time"),
            estimator
                .preview(observation(1, 111, 201), observation(1, 121, 201))
                .expect_err("delta outside policy"),
        ] {
            assert!(matches!(
                error,
                ClockRateEstimatorError::AttemptEpochChanged { .. }
                    | ClockRateEstimatorError::RepeatedFramePosition { .. }
                    | ClockRateEstimatorError::FramePositionRegression { .. }
                    | ClockRateEstimatorError::RepeatedMonotonicTime { .. }
                    | ClockRateEstimatorError::FrameDeltaOutsideBounds { .. }
            ));
            assert_eq!(
                (estimator.ratio(), estimator.latest_input(), estimator.latest_output()),
                initial
            );
        }

        assert!(matches!(
            estimator.preview(observation(1, 11, 101), observation(2, 21, 101)),
            Err(ClockRateEstimatorError::CrossClockEpoch { .. })
        ));
        assert_eq!(
            (estimator.ratio(), estimator.latest_input(), estimator.latest_output()),
            initial
        );
    }

    #[test]
    fn skew_and_out_of_bounds_rate_are_rejected_without_clamping() {
        let mut skewed = ClockRateEstimator::new(config(1, 100_000));
        assert!(matches!(
            skewed.preview(observation(1, 0, 0), observation(1, 0, NANOSECONDS_PER_SECOND + 1)),
            Err(ClockRateEstimatorError::ObservationSkew { .. })
        ));

        let narrow = RateMatchRatioBounds::try_new(2.0, 0.99, 1.01).expect("narrow bounds");
        let deltas = ClockDeltaBounds::try_new(1, 100_000, 1, 2 * NANOSECONDS_PER_SECOND)
            .expect("delta bounds");
        let mut estimator = ClockRateEstimator::new(ClockRateEstimatorConfig::new(
            deltas,
            deltas,
            NANOSECONDS_PER_SECOND,
            narrow,
        ));
        estimator.preview(observation(1, 0, 0), observation(1, 0, 0)).expect("anchor").commit();
        assert!(matches!(
            estimator.preview(
                observation(1, 48_000, NANOSECONDS_PER_SECOND),
                observation(1, 60_000, NANOSECONDS_PER_SECOND)
            ),
            Err(ClockRateEstimatorError::RatioOutsideBounds { .. })
        ));
        assert_eq!(estimator.ratio(), None);
    }
}
