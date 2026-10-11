//! Unpaced software processing measurements. Opens no device or audio service.
use librewave_core::{EndpointId, FaderGain, MixRoute, SourceControls, SourceId};
use librewave_engine::{
    CHANNELS, InputBuffer, MAX_SOURCES, MixerConfig, MixerEngine, OutputBuffer,
    RateMatchRatioBounds, RateMatcher, RateMatcherConfig, SAMPLE_RATE_HZ,
};
use serde::Serialize;
use std::{error::Error, hint::black_box, time::Instant};

const WARMUP_BLOCKS: usize = 128;
const MEASURED_BLOCKS: usize = 4_096;
const QUANTA: [usize; 5] = [64, 128, 256, 512, 1_024];

#[derive(Debug, Serialize)]
struct Timing {
    blocks: usize,
    minimum_ns: u64,
    median_ns: u64,
    p99_ns: u64,
    maximum_ns: u64,
    audio_period_ns: u64,
    period_overruns: usize,
}

impl Timing {
    fn from_samples(mut samples: Vec<u64>, frames: usize) -> Result<Self, Box<dyn Error>> {
        if samples.is_empty() || frames == 0 {
            return Err("timing needs samples and a nonzero frame count".into());
        }
        samples.sort_unstable();
        let period = u64::try_from(frames)? * 1_000_000_000 / u64::from(SAMPLE_RATE_HZ);
        let percentile = |percent: usize| samples[(samples.len() * percent).div_ceil(100) - 1];
        Ok(Self {
            blocks: samples.len(),
            minimum_ns: samples[0],
            median_ns: percentile(50),
            p99_ns: percentile(99),
            maximum_ns: samples[samples.len() - 1],
            audio_period_ns: period,
            period_overruns: samples.iter().filter(|&&sample| sample > period).count(),
        })
    }
}

#[derive(Debug, Serialize)]
struct Measurement {
    operation: &'static str,
    frames: usize,
    sources: Option<usize>,
    channels: usize,
    relative_ratio: Option<f64>,
    // A dependency reports this delay; it is not a hardware measurement.
    dependency_output_delay_frames: Option<usize>,
    timing: Timing,
}

fn measure_mixer(
    frames: usize,
    count: usize,
    blocks: usize,
) -> Result<Measurement, Box<dyn Error>> {
    let sources: Vec<_> =
        (1..=count).map(|id| u16::try_from(id).map(SourceId::new)).collect::<Result<_, _>>()?;
    let route = MixRoute::new(true, FaderGain::UNITY);
    let controls: Vec<_> =
        sources.iter().map(|&id| SourceControls::new(id, route, route)).collect();
    let (mut engine, _stager) =
        MixerEngine::new(MixerConfig::try_new(frames, sources[0], &sources)?, &controls)?;
    // Exact binary fractions keep the expected sum independent of rounding.
    let signal = vec![0.03125; frames * CHANNELS];
    let inputs: Vec<_> = sources.iter().map(|&id| InputBuffer::new(id, &signal)).collect();
    let mut microphone = vec![0.0; signal.len()];
    let mut monitor = vec![0.0; signal.len()];
    let mut stream = vec![0.0; signal.len()];
    let mut outputs = [
        OutputBuffer::new(EndpointId::Microphone, &mut microphone),
        OutputBuffer::new(EndpointId::MonitorMix, &mut monitor),
        OutputBuffer::new(EndpointId::StreamMix, &mut stream),
    ];
    for _ in 0..WARMUP_BLOCKS {
        black_box(engine.process(frames, black_box(&inputs), black_box(&mut outputs))?);
    }
    let mut samples = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        let start = Instant::now();
        let report = engine.process(frames, black_box(&inputs), black_box(&mut outputs));
        let elapsed = start.elapsed();
        black_box(report?);
        samples.push(u64::try_from(elapsed.as_nanos())?);
    }
    let sum = f32::from(u16::try_from(count)?) * 0.03125;
    if microphone.iter().any(|v| v.to_bits() != 0.03125_f32.to_bits())
        || monitor.iter().chain(&stream).any(|v| v.to_bits() != sum.to_bits())
    {
        return Err("mixer output integrity check failed".into());
    }
    Ok(Measurement {
        operation: "mixer_with_meters",
        frames,
        sources: Some(count),
        channels: CHANNELS,
        relative_ratio: None,
        dependency_output_delay_frames: None,
        timing: Timing::from_samples(samples, frames)?,
    })
}

fn measure_asrc(
    frames: usize,
    channels: usize,
    blocks: usize,
) -> Result<Measurement, Box<dyn Error>> {
    let ratio = if channels == 1 { 0.9998 } else { 1.0002 };
    let bounds = RateMatchRatioBounds::try_new(2.0, 0.98, 1.02)?;
    let mut matcher = RateMatcher::new(RateMatcherConfig::try_new(frames, channels, bounds)?)?;
    matcher.warm()?;
    let delay = matcher.output_delay_frames();
    let input = vec![0.125; matcher.maximum_input_frames() * channels];
    let mut output = vec![0.0; frames * channels];
    let mut samples = Vec::with_capacity(blocks);
    for block in 0..WARMUP_BLOCKS + blocks {
        let start = Instant::now();
        let prepared = matcher.prepare(frames, ratio, black_box(&mut output))?;
        let used = prepared.input_frames() * channels;
        let report = prepared.process(black_box(&input[..used]));
        let elapsed = start.elapsed();
        let report = black_box(report?);
        if report.output_frames() != frames {
            return Err("ASRC returned an incomplete output block".into());
        }
        if block >= WARMUP_BLOCKS {
            samples.push(u64::try_from(elapsed.as_nanos())?);
        }
    }
    if output.iter().any(|v| !v.is_finite() || (*v - 0.125).abs() > 0.00001) {
        return Err("ASRC output integrity check failed".into());
    }
    Ok(Measurement {
        operation: "sinc_asrc",
        frames,
        sources: None,
        channels,
        relative_ratio: Some(ratio),
        dependency_output_delay_frames: Some(delay),
        timing: Timing::from_samples(samples, frames)?,
    })
}

#[derive(Serialize)]
struct Report {
    schema: u8,
    scope: &'static str,
    hardware_round_trip_measured: bool,
    optimized: bool,
    architecture: &'static str,
    sample_rate_hz: u32,
    warmup_blocks: usize,
    measurements: Vec<Measurement>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut measurements = Vec::new();
    for frames in QUANTA {
        for sources in [2, MAX_SOURCES] {
            measurements.push(measure_mixer(frames, sources, MEASURED_BLOCKS)?);
        }
        for channels in [1, 2] {
            measurements.push(measure_asrc(frames, channels, MEASURED_BLOCKS)?);
        }
    }
    let report = Report {
        schema: 1,
        scope: "unpaced software calls; includes timer overhead and scheduler preemption; excludes PCM, FIFO, PipeWire scheduling, USB and acoustic paths",
        hardware_round_trip_measured: false,
        optimized: !cfg!(debug_assertions),
        architecture: std::env::consts::ARCH,
        sample_rate_hz: SAMPLE_RATE_HZ,
        warmup_blocks: WARMUP_BLOCKS,
        measurements,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_overruns_include_rare_stalls() {
        let timing = Timing::from_samples(vec![1, 7_000_000, 3, 2], 256).expect("timing");
        assert_eq!(timing.median_ns, 2);
        assert_eq!(timing.p99_ns, 7_000_000);
        assert_eq!(timing.maximum_ns, 7_000_000);
        assert_eq!(timing.audio_period_ns, 5_333_333);
        assert_eq!(timing.period_overruns, 1);
        assert!(Timing::from_samples(vec![], 256).is_err());
        assert!(Timing::from_samples(vec![1], 0).is_err());
    }

    #[test]
    fn measurement_runs_real_mixer_and_sinc_processing() {
        assert_eq!(measure_mixer(64, MAX_SOURCES, 4).expect("mixer").timing.blocks, 4);
        for channels in [1, 2] {
            let measured = measure_asrc(64, channels, 4).expect("ASRC");
            assert_eq!(measured.timing.blocks, 4);
            assert!(measured.dependency_output_delay_frames.is_some_and(|v| v > 0));
        }
    }
}
