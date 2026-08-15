use crate::OUTPUT_COUNT;
use crate::config::{CHANNELS, MAX_SOURCES, MixerConfig};
use crate::meter::{BlockMeters, StereoAccumulator, finish_block, mixer_output_index};
use crate::transfer::{
    ControlConsumer, ControlMappingError, ControlSnapshot, ControlStager, compile_snapshot,
};
use librewave_core::{EndpointId, SourceControls, SourceId};
use std::fmt;

/// One explicitly identified stereo interleaved input buffer.
#[derive(Clone, Copy, Debug)]
pub struct InputBuffer<'a> {
    source: SourceId,
    samples: &'a [f32],
}

impl<'a> InputBuffer<'a> {
    #[must_use]
    pub const fn new(source: SourceId, samples: &'a [f32]) -> Self {
        Self { source, samples }
    }

    #[must_use]
    pub const fn source(self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn samples(self) -> &'a [f32] {
        self.samples
    }
}

/// One explicitly identified stereo interleaved output buffer.
#[derive(Debug)]
pub struct OutputBuffer<'a> {
    endpoint: EndpointId,
    samples: &'a mut [f32],
}

impl<'a> OutputBuffer<'a> {
    #[must_use]
    pub const fn new(endpoint: EndpointId, samples: &'a mut [f32]) -> Self {
        Self { endpoint, samples }
    }

    #[must_use]
    pub const fn endpoint(&self) -> EndpointId {
        self.endpoint
    }
}

/// The fixed-capacity portable mixer.
///
/// Construction allocates the SPSC control slot. After construction,
/// [`Self::process`] does not allocate, lock, block, log, spawn, perform I/O,
/// call platform code, or use trait-object dispatch. It also does not clone or
/// drop the Arc that owns the handoff slot.
#[derive(Debug)]
pub struct MixerEngine {
    config: MixerConfig,
    controls: ControlSnapshot,
    control_consumer: ControlConsumer,
}

impl MixerEngine {
    /// Constructs a mixer with one coherent initial control snapshot.
    ///
    /// Fader coefficients are calculated during construction.
    ///
    /// # Errors
    ///
    /// Returns an error unless the initial controls identify each configured
    /// source exactly once.
    pub fn new(
        config: MixerConfig,
        initial_controls: &[SourceControls],
    ) -> Result<(Self, ControlStager), ControlMappingError> {
        let controls = compile_snapshot(&config, initial_controls)?;
        let (stager, control_consumer) = ControlConsumer::channel(config);
        Ok((Self { config, controls, control_consumer }, stager))
    }

    #[must_use]
    pub const fn config(&self) -> &MixerConfig {
        &self.config
    }

    /// Processes one coherent block with a negotiated variable frame count.
    ///
    /// The call validates every identity, length, and input sample before it
    /// changes an output or consumes a pending control snapshot. Finite input
    /// samples may exceed unity. Each endpoint sample is clipped to the
    /// inclusive range `-1.0..=1.0` after its complete sum is calculated.
    ///
    /// The microphone endpoint receives its designated source at unity and is
    /// independent of both mix routes. Monitor and stream use their own route
    /// enable and fader values.
    ///
    /// Zero frames is accepted when every buffer is empty. That call returns
    /// zero meters and leaves a pending control snapshot untouched.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError`] for an overflowing or excessive frame count,
    /// an incoherent buffer mapping, an inexact buffer length, or a non-finite
    /// input sample.
    pub fn process(
        &mut self,
        actual_frames: usize,
        inputs: &[InputBuffer<'_>],
        outputs: &mut [OutputBuffer<'_>],
    ) -> Result<ProcessReport, ProcessError> {
        let sample_count = actual_frames
            .checked_mul(CHANNELS)
            .ok_or(ProcessError::FrameSampleCountOverflow { actual_frames })?;
        if actual_frames > self.config.max_frames() {
            return Err(ProcessError::FrameCountExceedsMaximum {
                maximum: self.config.max_frames(),
                actual: actual_frames,
            });
        }

        let input_indices = validate_inputs(&self.config, inputs, sample_count)?;
        let output_indices = validate_outputs(outputs, sample_count)?;
        if actual_frames == 0 {
            return Ok(ProcessReport {
                control_update_applied: false,
                meters: BlockMeters::zero(&self.config),
            });
        }

        let control_update_applied = if let Some(controls) = self.control_consumer.take() {
            self.controls = controls;
            true
        } else {
            false
        };
        let mut source_meters = [StereoAccumulator::default(); MAX_SOURCES];
        let mut endpoint_meters = [StereoAccumulator::default(); OUTPUT_COUNT];
        let microphone_index = self.config.microphone_index();
        let microphone_output = validated_output_index(&output_indices, EndpointId::Microphone);
        let monitor_output = validated_output_index(&output_indices, EndpointId::MonitorMix);
        let stream_output = validated_output_index(&output_indices, EndpointId::StreamMix);
        let microphone_meter = configured_output_index(EndpointId::Microphone);
        let monitor_meter = configured_output_index(EndpointId::MonitorMix);
        let stream_meter = configured_output_index(EndpointId::StreamMix);

        for sample_index in 0..sample_count {
            let channel = sample_index % CHANNELS;
            let microphone = inputs[input_indices[microphone_index]].samples[sample_index];
            let mut monitor = 0.0_f64;
            let mut stream = 0.0_f64;

            for source_index in 0..self.config.sources().len() {
                let sample = inputs[input_indices[source_index]].samples[sample_index];
                source_meters[source_index].observe(channel, sample);
                monitor +=
                    f64::from(sample) * f64::from(self.controls.monitor[source_index].coefficient);
                stream +=
                    f64::from(sample) * f64::from(self.controls.stream[source_index].coefficient);
            }

            let microphone = microphone.clamp(-1.0, 1.0);
            let monitor = clip_mix(monitor);
            let stream = clip_mix(stream);
            outputs[microphone_output].samples[sample_index] = microphone;
            outputs[monitor_output].samples[sample_index] = monitor;
            outputs[stream_output].samples[sample_index] = stream;
            endpoint_meters[microphone_meter].observe(channel, microphone);
            endpoint_meters[monitor_meter].observe(channel, monitor);
            endpoint_meters[stream_meter].observe(channel, stream);
        }

        Ok(ProcessReport {
            control_update_applied,
            meters: finish_block(&self.config, &source_meters, &endpoint_meters),
        })
    }
}

#[allow(clippy::cast_possible_truncation)]
fn clip_mix(sample: f64) -> f32 {
    // Clipping proves that the f64 value is finite and inside the f32 range.
    sample.clamp(-1.0, 1.0) as f32
}

fn validated_output_index(output_indices: &[usize; OUTPUT_COUNT], endpoint: EndpointId) -> usize {
    output_indices[configured_output_index(endpoint)]
}

fn configured_output_index(endpoint: EndpointId) -> usize {
    mixer_output_index(endpoint).expect("engine output is present in MIXER_OUTPUT_ENDPOINTS")
}

/// The fixed-capacity result of one accepted block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessReport {
    control_update_applied: bool,
    meters: BlockMeters,
}

impl ProcessReport {
    #[must_use]
    pub const fn control_update_applied(self) -> bool {
        self.control_update_applied
    }

    #[must_use]
    pub const fn meters(self) -> BlockMeters {
        self.meters
    }
}

/// Why a processing call was rejected before output mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessError {
    FrameSampleCountOverflow { actual_frames: usize },
    FrameCountExceedsMaximum { maximum: usize, actual: usize },
    InputCount { expected: usize, actual: usize },
    OutputCount { expected: usize, actual: usize },
    UnknownInput(SourceId),
    DuplicateInput(SourceId),
    EndpointIsNotMixerOutput(EndpointId),
    DuplicateOutput(EndpointId),
    InputLength { source: SourceId, expected: usize, actual: usize },
    OutputLength { endpoint: EndpointId, expected: usize, actual: usize },
    NonFiniteInput { source: SourceId, sample_index: usize },
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameSampleCountOverflow { actual_frames } => {
                write!(formatter, "{actual_frames} frames overflow the interleaved sample count")
            }
            Self::FrameCountExceedsMaximum { maximum, actual } => {
                write!(formatter, "{actual} frames exceed the configured maximum of {maximum}")
            }
            Self::InputCount { expected, actual } => {
                write!(formatter, "expected {expected} input buffers, received {actual}")
            }
            Self::OutputCount { expected, actual } => {
                write!(formatter, "expected {expected} output buffers, received {actual}")
            }
            Self::UnknownInput(source) => write!(formatter, "source {source} is not configured"),
            Self::DuplicateInput(source) => write!(formatter, "duplicate source {source} input"),
            Self::EndpointIsNotMixerOutput(endpoint) => {
                write!(formatter, "{} is not a mixer output", endpoint.display_name())
            }
            Self::DuplicateOutput(endpoint) => {
                write!(formatter, "duplicate {} output", endpoint.display_name())
            }
            Self::InputLength { source, expected, actual } => {
                write!(formatter, "source {source} input has {actual} samples; expected {expected}")
            }
            Self::OutputLength { endpoint, expected, actual } => write!(
                formatter,
                "{} output has {actual} samples; expected {expected}",
                endpoint.display_name()
            ),
            Self::NonFiniteInput { source, sample_index } => {
                write!(formatter, "source {source} sample {sample_index} is not finite")
            }
        }
    }
}

impl std::error::Error for ProcessError {}

fn validate_inputs(
    config: &MixerConfig,
    inputs: &[InputBuffer<'_>],
    sample_count: usize,
) -> Result<[usize; MAX_SOURCES], ProcessError> {
    if inputs.len() != config.sources().len() {
        return Err(ProcessError::InputCount {
            expected: config.sources().len(),
            actual: inputs.len(),
        });
    }

    let mut indices = [0; MAX_SOURCES];
    let mut present = [false; MAX_SOURCES];
    for (buffer_index, input) in inputs.iter().enumerate() {
        let Some(source_index) = config.source_index(input.source) else {
            return Err(ProcessError::UnknownInput(input.source));
        };
        if present[source_index] {
            return Err(ProcessError::DuplicateInput(input.source));
        }
        if input.samples.len() != sample_count {
            return Err(ProcessError::InputLength {
                source: input.source,
                expected: sample_count,
                actual: input.samples.len(),
            });
        }
        for (sample_index, sample) in input.samples.iter().copied().enumerate() {
            if !sample.is_finite() {
                return Err(ProcessError::NonFiniteInput { source: input.source, sample_index });
            }
        }
        present[source_index] = true;
        indices[source_index] = buffer_index;
    }
    Ok(indices)
}

fn validate_outputs(
    outputs: &[OutputBuffer<'_>],
    sample_count: usize,
) -> Result<[usize; OUTPUT_COUNT], ProcessError> {
    if outputs.len() != OUTPUT_COUNT {
        return Err(ProcessError::OutputCount { expected: OUTPUT_COUNT, actual: outputs.len() });
    }

    let mut indices = [0; OUTPUT_COUNT];
    let mut present = [false; OUTPUT_COUNT];
    for (buffer_index, output) in outputs.iter().enumerate() {
        let Some(endpoint_index) = mixer_output_index(output.endpoint) else {
            return Err(ProcessError::EndpointIsNotMixerOutput(output.endpoint));
        };
        if present[endpoint_index] {
            return Err(ProcessError::DuplicateOutput(output.endpoint));
        }
        if output.samples.len() != sample_count {
            return Err(ProcessError::OutputLength {
                endpoint: output.endpoint,
                expected: sample_count,
                actual: output.samples.len(),
            });
        }
        present[endpoint_index] = true;
        indices[endpoint_index] = buffer_index;
    }
    Ok(indices)
}
