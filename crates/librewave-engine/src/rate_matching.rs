//! Portable adaptive sample-rate matching for bounded mono or stereo streams.
//!
//! Rubato is intentionally confined to this module. Callers own interleaved
//! buffers. The matcher owns the resampler and preallocated planar scratch.
//! The controller owns one stream's feedback state and can be instantiated
//! independently for capture and playback.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters};
use std::fmt;

const NOMINAL_RATIO: f64 = 1.0;

/// Smallest output capacity accepted before the platform's quantum policy.
pub const MIN_RATE_MATCH_RESOURCE_OUTPUT_FRAMES: usize = 1;

/// Largest output capacity allowed by the portable dependency-safety budget.
///
/// With rubato 4.0.0's default 256-point sinc filter and a dependency ratio
/// limit of 2.0, `input_frames_max()` is about 2178 frames at the maximum
/// output size. Rubato's own history buffer adds `2 * sinc_len`, for about
/// 2690 frames per channel in total.
pub const MAX_RATE_MATCH_RESOURCE_OUTPUT_FRAMES: usize = 1_024;

/// Smallest dependency-relative ratio accepted by the resource budget.
pub const MIN_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO: f64 = 0.5;

/// Largest dependency-relative ratio accepted by the resource budget.
pub const MAX_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO: f64 = 2.0;

/// Checked relative-ratio bounds shared by a matcher and its controller.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateMatchRatioBounds {
    dependency_ratio_limit: f64,
    minimum: f64,
    maximum: f64,
}

impl RateMatchRatioBounds {
    /// Constructs product correction bounds within rubato's dependency limit.
    ///
    /// The dependency limit is deliberately separate from product bounds. It
    /// is a memory-safety ceiling for the pinned dependency. The product
    /// bounds are caller-selected policy.
    ///
    /// # Errors
    ///
    /// Returns an error for non-finite, unordered, non-positive, unsupported,
    /// or dependency-incompatible bounds.
    pub fn try_new(
        dependency_ratio_limit: f64,
        minimum: f64,
        maximum: f64,
    ) -> Result<Self, RateMatchRatioBoundsError> {
        if !dependency_ratio_limit.is_finite()
            || !(1.0..=MAX_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO).contains(&dependency_ratio_limit)
        {
            return Err(RateMatchRatioBoundsError::InvalidDependencyLimit {
                actual: dependency_ratio_limit,
            });
        }
        if !minimum.is_finite()
            || !maximum.is_finite()
            || minimum < MIN_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO
            || maximum > MAX_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO
            || minimum <= 0.0
            || minimum > 1.0
            || maximum < 1.0
            || minimum > maximum
        {
            return Err(RateMatchRatioBoundsError::InvalidProductBounds { minimum, maximum });
        }
        if minimum < 1.0 / dependency_ratio_limit || maximum > dependency_ratio_limit {
            return Err(RateMatchRatioBoundsError::ProductBoundsExceedDependency {
                minimum,
                maximum,
                dependency_limit: dependency_ratio_limit,
            });
        }
        Ok(Self { dependency_ratio_limit, minimum, maximum })
    }

    #[must_use]
    pub const fn dependency_ratio_limit(self) -> f64 {
        self.dependency_ratio_limit
    }

    #[must_use]
    pub const fn minimum(self) -> f64 {
        self.minimum
    }

    #[must_use]
    pub const fn maximum(self) -> f64 {
        self.maximum
    }
}

/// Why ratio bounds were rejected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateMatchRatioBoundsError {
    InvalidDependencyLimit { actual: f64 },
    InvalidProductBounds { minimum: f64, maximum: f64 },
    ProductBoundsExceedDependency { minimum: f64, maximum: f64, dependency_limit: f64 },
}

impl fmt::Display for RateMatchRatioBoundsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDependencyLimit { actual } => write!(
                formatter,
                "dependency ratio limit must be finite and in 1.0..={MAX_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO}: {actual}"
            ),
            Self::InvalidProductBounds { minimum, maximum } => write!(
                formatter,
                "product ratio bounds must be finite, ordered, and in {MIN_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO}..={MAX_RATE_MATCH_DEPENDENCY_RELATIVE_RATIO}: {minimum}..={maximum}"
            ),
            Self::ProductBoundsExceedDependency { minimum, maximum, dependency_limit } => write!(
                formatter,
                "product ratio bounds {minimum}..={maximum} exceed dependency limit {dependency_limit}"
            ),
        }
    }
}

impl std::error::Error for RateMatchRatioBoundsError {}

/// Configuration for one matcher. The channel count is exactly 1 or 2.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateMatcherConfig {
    max_output_frames: usize,
    channels: usize,
    ratio_bounds: RateMatchRatioBounds,
}

impl RateMatcherConfig {
    /// Constructs a checked matcher capacity and channel shape.
    ///
    /// `max_output_frames` is an engine resource ceiling. A platform may
    /// choose a smaller operating quantum for a particular attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the capacity exceeds the dependency-safety
    /// ceiling or channels is not 1 or 2.
    pub fn try_new(
        max_output_frames: usize,
        channels: usize,
        ratio_bounds: RateMatchRatioBounds,
    ) -> Result<Self, RateMatcherConfigError> {
        if !(MIN_RATE_MATCH_RESOURCE_OUTPUT_FRAMES..=MAX_RATE_MATCH_RESOURCE_OUTPUT_FRAMES)
            .contains(&max_output_frames)
        {
            return Err(RateMatcherConfigError::OutputCapacityOutsideResourceLimit {
                actual: max_output_frames,
            });
        }
        if !matches!(channels, 1 | 2) {
            return Err(RateMatcherConfigError::UnsupportedChannelCount { actual: channels });
        }
        Ok(Self { max_output_frames, channels, ratio_bounds })
    }

    #[must_use]
    pub const fn max_output_frames(self) -> usize {
        self.max_output_frames
    }

    #[must_use]
    pub const fn channels(self) -> usize {
        self.channels
    }

    #[must_use]
    pub const fn ratio_bounds(self) -> RateMatchRatioBounds {
        self.ratio_bounds
    }
}

/// Why matcher capacity or shape was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateMatcherConfigError {
    OutputCapacityOutsideResourceLimit { actual: usize },
    UnsupportedChannelCount { actual: usize },
}

impl fmt::Display for RateMatcherConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputCapacityOutsideResourceLimit { actual } => write!(
                formatter,
                "maximum output frames must be in {MIN_RATE_MATCH_RESOURCE_OUTPUT_FRAMES}..={MAX_RATE_MATCH_RESOURCE_OUTPUT_FRAMES}: {actual}"
            ),
            Self::UnsupportedChannelCount { actual } => {
                write!(formatter, "rate matcher supports exactly 1 or 2 channels: {actual}")
            }
        }
    }
}

impl std::error::Error for RateMatcherConfigError {}

/// Configuration for one stateful realtime-safe fill controller.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateMatchControllerConfig {
    target_fill_frames: usize,
    proportional_gain: f64,
    integral_gain: f64,
    integral_limit: f64,
    slew_limit: f64,
    ratio_bounds: RateMatchRatioBounds,
}

impl RateMatchControllerConfig {
    /// Constructs checked controller gains, slew, integral, and shared bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero target or non-finite or negative control
    /// parameters.
    pub fn try_new(
        target_fill_frames: usize,
        proportional_gain: f64,
        integral_gain: f64,
        integral_limit: f64,
        slew_limit: f64,
        ratio_bounds: RateMatchRatioBounds,
    ) -> Result<Self, RateMatchControllerError> {
        if target_fill_frames == 0 {
            return Err(RateMatchControllerError::ZeroTargetFill);
        }
        if !proportional_gain.is_finite() || proportional_gain < 0.0 {
            return Err(RateMatchControllerError::InvalidProportionalGain(proportional_gain));
        }
        if !integral_gain.is_finite() || integral_gain < 0.0 {
            return Err(RateMatchControllerError::InvalidIntegralGain(integral_gain));
        }
        if !integral_limit.is_finite() || integral_limit <= 0.0 {
            return Err(RateMatchControllerError::InvalidIntegralLimit(integral_limit));
        }
        if !slew_limit.is_finite() || slew_limit < 0.0 {
            return Err(RateMatchControllerError::InvalidSlewLimit(slew_limit));
        }
        Ok(Self {
            target_fill_frames,
            proportional_gain,
            integral_gain,
            integral_limit,
            slew_limit,
            ratio_bounds,
        })
    }
}

/// Why controller configuration or preview was rejected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateMatchControllerError {
    ZeroTargetFill,
    InvalidProportionalGain(f64),
    InvalidIntegralGain(f64),
    InvalidIntegralLimit(f64),
    InvalidSlewLimit(f64),
    NonFinitePreview {
        stage: RateMatchControllerArithmetic,
    },
    PreviewOutsideBounds {
        stage: RateMatchControllerArithmetic,
        actual: f64,
        minimum: f64,
        maximum: f64,
    },
}

/// The intermediate arithmetic stage that rejected a controller preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateMatchControllerArithmetic {
    IntegralHistory,
    FillDifference,
    FillError,
    IntegralAccumulation,
    Integral,
    ProportionalTerm,
    IntegralTerm,
    NominalPlusProportional,
    DesiredSum,
    DesiredRatio,
    SlewDelta,
    SlewStep,
    RatioSum,
    RelativeRatio,
}

fn finite_preview(
    value: f64,
    stage: RateMatchControllerArithmetic,
) -> Result<f64, RateMatchControllerError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(RateMatchControllerError::NonFinitePreview { stage })
    }
}

fn bounded_preview(
    value: f64,
    minimum: f64,
    maximum: f64,
    stage: RateMatchControllerArithmetic,
) -> Result<f64, RateMatchControllerError> {
    let value = finite_preview(value, stage)?;
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(RateMatchControllerError::PreviewOutsideBounds {
            stage,
            actual: value,
            minimum,
            maximum,
        })
    }
}

impl fmt::Display for RateMatchControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTargetFill => formatter.write_str("target fill must be nonzero"),
            Self::InvalidProportionalGain(value) => {
                write!(formatter, "proportional gain is invalid: {value}")
            }
            Self::InvalidIntegralGain(value) => {
                write!(formatter, "integral gain is invalid: {value}")
            }
            Self::InvalidIntegralLimit(value) => {
                write!(formatter, "integral limit is invalid: {value}")
            }
            Self::InvalidSlewLimit(value) => write!(formatter, "slew limit is invalid: {value}"),
            Self::NonFinitePreview { stage } => {
                write!(formatter, "controller preview arithmetic is non-finite at {stage:?}")
            }
            Self::PreviewOutsideBounds { stage, actual, minimum, maximum } => write!(
                formatter,
                "controller preview value {actual} at {stage:?} is outside {minimum}..={maximum}"
            ),
        }
    }
}

impl std::error::Error for RateMatchControllerError {}

/// Stateful, allocation-free feedback controller.
#[derive(Clone, Debug)]
pub struct RateMatchController {
    config: RateMatchControllerConfig,
    integral: f64,
    previous_ratio: f64,
}

impl RateMatchController {
    #[must_use]
    pub fn new(config: RateMatchControllerConfig) -> Self {
        Self { config, integral: 0.0, previous_ratio: NOMINAL_RATIO }
    }

    /// Previews a candidate without changing integral or ratio history.
    ///
    /// The returned step must be committed only after the corresponding audio
    /// cycle succeeds. It mutably borrows this controller, so another preview,
    /// reset, or commit cannot be interleaved. Dropping it leaves this
    /// controller unchanged.
    /// # Errors
    ///
    /// Returns an arithmetic or bounds error without changing controller
    /// history when a finite configuration produces an invalid intermediate.
    #[allow(clippy::cast_precision_loss)]
    pub fn preview(
        &mut self,
        fill_frames: usize,
    ) -> Result<RateMatchControllerStep<'_>, RateMatchControllerError> {
        let target = self.config.target_fill_frames as f64;
        let history_integral = bounded_preview(
            self.integral,
            -self.config.integral_limit,
            self.config.integral_limit,
            RateMatchControllerArithmetic::IntegralHistory,
        )?;
        let history_ratio = bounded_preview(
            self.previous_ratio,
            self.config.ratio_bounds.minimum,
            self.config.ratio_bounds.maximum,
            RateMatchControllerArithmetic::RelativeRatio,
        )?;
        let difference = finite_preview(
            target - fill_frames as f64,
            RateMatchControllerArithmetic::FillDifference,
        )?;
        let error = finite_preview(difference / target, RateMatchControllerArithmetic::FillError)?;
        let integral_accumulation = finite_preview(
            history_integral + error,
            RateMatchControllerArithmetic::IntegralAccumulation,
        )?;
        let integral = bounded_preview(
            integral_accumulation.clamp(-self.config.integral_limit, self.config.integral_limit),
            -self.config.integral_limit,
            self.config.integral_limit,
            RateMatchControllerArithmetic::Integral,
        )?;
        let proportional_term = finite_preview(
            self.config.proportional_gain * error,
            RateMatchControllerArithmetic::ProportionalTerm,
        )?;
        let integral_term = finite_preview(
            self.config.integral_gain * integral,
            RateMatchControllerArithmetic::IntegralTerm,
        )?;
        let nominal_plus_proportional = finite_preview(
            NOMINAL_RATIO + proportional_term,
            RateMatchControllerArithmetic::NominalPlusProportional,
        )?;
        let desired_sum = finite_preview(
            nominal_plus_proportional + integral_term,
            RateMatchControllerArithmetic::DesiredSum,
        )?;
        let desired = bounded_preview(
            desired_sum.clamp(self.config.ratio_bounds.minimum, self.config.ratio_bounds.maximum),
            self.config.ratio_bounds.minimum,
            self.config.ratio_bounds.maximum,
            RateMatchControllerArithmetic::DesiredRatio,
        )?;
        let slew_delta =
            finite_preview(desired - history_ratio, RateMatchControllerArithmetic::SlewDelta)?;
        let slew_step = finite_preview(
            slew_delta.clamp(-self.config.slew_limit, self.config.slew_limit),
            RateMatchControllerArithmetic::SlewStep,
        )?;
        let ratio_sum =
            finite_preview(history_ratio + slew_step, RateMatchControllerArithmetic::RatioSum)?;
        let ratio = bounded_preview(
            ratio_sum.clamp(self.config.ratio_bounds.minimum, self.config.ratio_bounds.maximum),
            self.config.ratio_bounds.minimum,
            self.config.ratio_bounds.maximum,
            RateMatchControllerArithmetic::RelativeRatio,
        )?;
        Ok(RateMatchControllerStep { controller: self, integral, ratio })
    }

    /// Clears controller history. A preview token must not be live here; its
    /// mutable borrow prevents reset in safe Rust.
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.previous_ratio = NOMINAL_RATIO;
    }

    #[must_use]
    pub const fn integral(&self) -> f64 {
        self.integral
    }

    #[must_use]
    pub const fn ratio(&self) -> f64 {
        self.previous_ratio
    }
}

/// A borrow-scoped, non-copyable candidate controller state.
pub struct RateMatchControllerStep<'a> {
    controller: &'a mut RateMatchController,
    integral: f64,
    ratio: f64,
}

impl RateMatchControllerStep<'_> {
    #[must_use]
    pub const fn relative_ratio(&self) -> f64 {
        self.ratio
    }

    /// Commits this candidate to the exact controller it borrowed.
    pub fn commit(self) {
        self.controller.integral = self.integral;
        self.controller.previous_ratio = self.ratio;
    }
}

impl fmt::Debug for RateMatchControllerStep<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RateMatchControllerStep")
            .field("integral", &self.integral)
            .field("ratio", &self.ratio)
            .finish()
    }
}

/// A fixed-output processing report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateMatchReport {
    input_frames: usize,
    output_frames: usize,
    relative_ratio: f64,
}

impl RateMatchReport {
    #[must_use]
    pub const fn input_frames(self) -> usize {
        self.input_frames
    }

    #[must_use]
    pub const fn output_frames(self) -> usize {
        self.output_frames
    }

    #[must_use]
    pub const fn relative_ratio(self) -> f64 {
        self.relative_ratio
    }
}

/// A sticky fault after a changed cycle cannot safely continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateMatchFault {
    DependencyRejected,
    UnexpectedFrameCount,
    NonFiniteOutput { channel: usize, frame: usize },
}

impl fmt::Display for RateMatchFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DependencyRejected => {
                formatter.write_str("the resampler rejected a changed cycle")
            }
            Self::UnexpectedFrameCount => {
                formatter.write_str("the resampler returned unexpected frame counts")
            }
            Self::NonFiniteOutput { channel, frame } => {
                write!(formatter, "resampler output channel {channel} frame {frame} is not finite")
            }
        }
    }
}

impl std::error::Error for RateMatchFault {}

/// Why a matcher operation was rejected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateMatchError {
    InvalidQuantum { actual: usize, maximum: usize },
    InvalidRelativeRatio { actual: f64, minimum: f64, maximum: f64 },
    NotWarmed,
    NeedsReset,
    Faulted(RateMatchFault),
    OutputLength { expected_samples: usize, actual_samples: usize },
    InputLength { expected_samples: usize, actual_samples: usize },
    NonFiniteInput { frame: usize, channel: usize },
}

impl RateMatchError {
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::InvalidQuantum { .. }
                | Self::InvalidRelativeRatio { .. }
                | Self::NotWarmed
                | Self::OutputLength { .. }
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::NeedsReset
                | Self::Faulted(_)
                | Self::InputLength { .. }
                | Self::NonFiniteInput { .. }
        )
    }
}

impl fmt::Display for RateMatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuantum { actual, maximum } => {
                write!(formatter, "output quantum {actual} is outside 1..={maximum}")
            }
            Self::InvalidRelativeRatio { actual, minimum, maximum } => {
                write!(formatter, "relative ratio {actual} is outside {minimum}..={maximum}")
            }
            Self::NotWarmed => {
                formatter.write_str("rate matcher must be warmed off realtime first")
            }
            Self::NeedsReset => formatter.write_str("rate matcher needs an off-realtime reset"),
            Self::Faulted(fault) => write!(formatter, "rate matcher is faulted: {fault}"),
            Self::OutputLength { expected_samples, actual_samples } => write!(
                formatter,
                "rate-match output has {actual_samples} samples; expected {expected_samples}"
            ),
            Self::InputLength { expected_samples, actual_samples } => write!(
                formatter,
                "rate-match input has {actual_samples} samples; expected {expected_samples}"
            ),
            Self::NonFiniteInput { frame, channel } => {
                write!(formatter, "rate-match input channel {channel} frame {frame} is not finite")
            }
        }
    }
}

impl std::error::Error for RateMatchError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RateMatcherLifecycle {
    Unwarmed,
    Ready,
    NeedsReset,
    Faulted(RateMatchFault),
}

/// One portable adaptive matcher with a fixed output quantum per cycle.
pub struct RateMatcher {
    config: RateMatcherConfig,
    resampler: Async<f32>,
    input_planar: Vec<Vec<f32>>,
    output_planar: Vec<Vec<f32>>,
    lifecycle: RateMatcherLifecycle,
}

impl RateMatcher {
    /// Constructs an unwarmed matcher and allocates all scratch off realtime.
    ///
    /// # Errors
    ///
    /// Returns a dependency fault if the pinned resampler rejects the checked
    /// resource configuration.
    pub fn new(config: RateMatcherConfig) -> Result<Self, RateMatchError> {
        let resampler = Async::<f32>::new_sinc(
            NOMINAL_RATIO,
            config.ratio_bounds.dependency_ratio_limit,
            // Rubato 4.0.0's default: 256-point sinc, automatic cutoff,
            // oversampling 128, cubic interpolation, BlackmanHarris2 window.
            &SincInterpolationParameters::default(),
            config.max_output_frames,
            config.channels,
            FixedAsync::Output,
        )
        .map_err(|_| RateMatchError::Faulted(RateMatchFault::DependencyRejected))?;
        let input_capacity = resampler.input_frames_max();
        let output_capacity = resampler.output_frames_max();
        let input_planar = (0..config.channels).map(|_| vec![0.0; input_capacity]).collect();
        let output_planar = (0..config.channels).map(|_| vec![0.0; output_capacity]).collect();
        Ok(Self {
            config,
            resampler,
            input_planar,
            output_planar,
            lifecycle: RateMatcherLifecycle::Unwarmed,
        })
    }

    #[must_use]
    pub const fn config(&self) -> RateMatcherConfig {
        self.config
    }

    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self.lifecycle, RateMatcherLifecycle::Ready)
    }

    #[must_use]
    pub const fn needs_reset(&self) -> bool {
        matches!(
            self.lifecycle,
            RateMatcherLifecycle::NeedsReset | RateMatcherLifecycle::Faulted(_)
        )
    }

    #[must_use]
    pub const fn fault(&self) -> Option<RateMatchFault> {
        match self.lifecycle {
            RateMatcherLifecycle::Faulted(fault) => Some(fault),
            RateMatcherLifecycle::Unwarmed
            | RateMatcherLifecycle::Ready
            | RateMatcherLifecycle::NeedsReset => None,
        }
    }

    /// Performs an actual zero-input warmup at the configured maximum off RT.
    ///
    /// The warmup validates the exact input/output counts and finite output,
    /// then resets the resampler to its nominal clean state.
    ///
    /// # Errors
    ///
    /// Returns a sticky dependency fault if warmup or its exact-count checks
    /// fail. This method itself resets the resampler and lifecycle, so retry it
    /// directly off realtime; [`Self::reset`] is only needed to leave the
    /// matcher unwarmed or before teardown.
    pub fn warm(&mut self) -> Result<(), RateMatchError> {
        self.clear_lifecycle();
        let input_frames = self.resampler.input_frames_next();
        for channel in 0..self.config.channels {
            self.input_planar[channel][..input_frames].fill(0.0);
        }
        let Ok(input_adapter) =
            SequentialSliceOfVecs::new(&self.input_planar, self.config.channels, input_frames)
        else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        let Ok(mut output_adapter) = SequentialSliceOfVecs::new_mut(
            &mut self.output_planar,
            self.config.channels,
            self.config.max_output_frames,
        ) else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        let Ok((actual_input_frames, actual_output_frames)) =
            self.resampler.process_into_buffer(&input_adapter, &mut output_adapter, None)
        else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        if actual_input_frames != input_frames
            || actual_output_frames != self.config.max_output_frames
        {
            return Err(self.poison(RateMatchFault::UnexpectedFrameCount));
        }
        for channel in 0..self.config.channels {
            for frame in 0..actual_output_frames {
                if !self.output_planar[channel][frame].is_finite() {
                    return Err(self.poison(RateMatchFault::NonFiniteOutput { channel, frame }));
                }
            }
        }
        self.resampler.reset();
        self.lifecycle = RateMatcherLifecycle::Ready;
        Ok(())
    }

    /// Clears the matcher to an unwarmed nominal state off realtime.
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.lifecycle = RateMatcherLifecycle::Unwarmed;
    }

    /// Prepares one output quantum and ramped relative ratio.
    ///
    /// Quantum, ratio, and output shape checks happen before rubato changes.
    /// After the first successful dependency mutation, every later failure or
    /// dropped token requires reset. The token exposes the exact input count
    /// before the caller copies its FIFO block.
    ///
    /// # Errors
    ///
    /// Returns a retryable validation error before mutation, or a terminal
    /// dependency error after mutation begins.
    pub fn prepare<'a>(
        &'a mut self,
        output_frames: usize,
        relative_ratio: f64,
        output: &'a mut [f32],
    ) -> Result<PreparedRateMatch<'a>, RateMatchError> {
        self.ensure_ready()?;
        if output_frames == 0 || output_frames > self.config.max_output_frames {
            return Err(RateMatchError::InvalidQuantum {
                actual: output_frames,
                maximum: self.config.max_output_frames,
            });
        }
        let bounds = self.config.ratio_bounds;
        if !relative_ratio.is_finite()
            || relative_ratio < bounds.minimum
            || relative_ratio > bounds.maximum
        {
            return Err(RateMatchError::InvalidRelativeRatio {
                actual: relative_ratio,
                minimum: bounds.minimum,
                maximum: bounds.maximum,
            });
        }
        let expected_output_samples = output_frames * self.config.channels;
        if output.len() != expected_output_samples {
            return Err(RateMatchError::OutputLength {
                expected_samples: expected_output_samples,
                actual_samples: output.len(),
            });
        }
        let Some(resizable) = self.resampler.as_resizable() else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        if resizable.set_chunk_size(output_frames).is_err() {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        }
        let Some(adjustable) = self.resampler.as_adjustable() else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        if adjustable.set_resample_ratio_relative(relative_ratio, true).is_err() {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        }
        self.lifecycle = RateMatcherLifecycle::NeedsReset;
        let input_frames = self.resampler.input_frames_next();
        Ok(PreparedRateMatch {
            matcher: self,
            output,
            input_frames,
            output_frames,
            consumed: false,
        })
    }

    fn process_prepared(
        &mut self,
        input_frames: usize,
        output_frames: usize,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<RateMatchReport, RateMatchError> {
        let expected_input_samples = input_frames * self.config.channels;
        if input.len() != expected_input_samples {
            self.mark_needs_reset();
            return Err(RateMatchError::InputLength {
                expected_samples: expected_input_samples,
                actual_samples: input.len(),
            });
        }
        for frame in 0..input_frames {
            let base = frame * self.config.channels;
            for channel in 0..self.config.channels {
                let sample = input[base + channel];
                if !sample.is_finite() {
                    self.mark_needs_reset();
                    return Err(RateMatchError::NonFiniteInput { frame, channel });
                }
                self.input_planar[channel][frame] = sample;
            }
        }
        let Ok(input_adapter) =
            SequentialSliceOfVecs::new(&self.input_planar, self.config.channels, input_frames)
        else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        let Ok(mut output_adapter) = SequentialSliceOfVecs::new_mut(
            &mut self.output_planar,
            self.config.channels,
            output_frames,
        ) else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        let Ok((actual_input_frames, actual_output_frames)) =
            self.resampler.process_into_buffer(&input_adapter, &mut output_adapter, None)
        else {
            return Err(self.poison(RateMatchFault::DependencyRejected));
        };
        if actual_input_frames != input_frames || actual_output_frames != output_frames {
            return Err(self.poison(RateMatchFault::UnexpectedFrameCount));
        }
        // Validate the complete planar block before touching caller-owned
        // output. This bounded second pass prevents a dependency-generated
        // non-finite sample from leaving a partially written public block.
        for channel in 0..self.config.channels {
            for frame in 0..output_frames {
                let sample = self.output_planar[channel][frame];
                if !sample.is_finite() {
                    return Err(self.poison(RateMatchFault::NonFiniteOutput { channel, frame }));
                }
            }
        }
        for frame in 0..output_frames {
            let base = frame * self.config.channels;
            for channel in 0..self.config.channels {
                output[base + channel] = self.output_planar[channel][frame];
            }
        }
        self.lifecycle = RateMatcherLifecycle::Ready;
        Ok(RateMatchReport {
            input_frames: actual_input_frames,
            output_frames: actual_output_frames,
            relative_ratio: self.resampler.resample_ratio(),
        })
    }

    fn clear_lifecycle(&mut self) {
        self.resampler.reset();
        self.lifecycle = RateMatcherLifecycle::Unwarmed;
    }

    fn ensure_ready(&self) -> Result<(), RateMatchError> {
        match self.lifecycle {
            RateMatcherLifecycle::Unwarmed => Err(RateMatchError::NotWarmed),
            RateMatcherLifecycle::Ready => Ok(()),
            RateMatcherLifecycle::NeedsReset => Err(RateMatchError::NeedsReset),
            RateMatcherLifecycle::Faulted(fault) => Err(RateMatchError::Faulted(fault)),
        }
    }

    fn mark_needs_reset(&mut self) {
        self.lifecycle = RateMatcherLifecycle::NeedsReset;
    }

    fn poison(&mut self, fault: RateMatchFault) -> RateMatchError {
        self.lifecycle = RateMatcherLifecycle::Faulted(fault);
        RateMatchError::Faulted(fault)
    }
}

/// A non-copyable matcher transaction. Dropping it requires matcher reset.
pub struct PreparedRateMatch<'a> {
    matcher: &'a mut RateMatcher,
    output: &'a mut [f32],
    input_frames: usize,
    output_frames: usize,
    consumed: bool,
}

impl PreparedRateMatch<'_> {
    #[must_use]
    pub const fn input_frames(&self) -> usize {
        self.input_frames
    }

    #[must_use]
    pub const fn output_frames(&self) -> usize {
        self.output_frames
    }

    /// Consumes the transaction with one exact interleaved input block.
    ///
    /// # Errors
    ///
    /// Returns a terminal error for an input shortage, non-finite input, or a
    /// dependency failure. The matcher then requires off-realtime reset.
    pub fn process(mut self, input: &[f32]) -> Result<RateMatchReport, RateMatchError> {
        let result = self.matcher.process_prepared(
            self.input_frames,
            self.output_frames,
            input,
            &mut *self.output,
        );
        self.consumed = true;
        result
    }
}

impl fmt::Debug for PreparedRateMatch<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedRateMatch")
            .field("input_frames", &self.input_frames)
            .field("output_frames", &self.output_frames)
            .field("consumed", &self.consumed)
            .finish()
    }
}

impl Drop for PreparedRateMatch<'_> {
    fn drop(&mut self) {
        if !self.consumed {
            self.matcher.mark_needs_reset();
        }
    }
}
