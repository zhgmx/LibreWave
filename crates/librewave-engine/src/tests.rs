#![allow(clippy::float_cmp)]

use super::*;
use librewave_core::{EndpointId, FaderGain, FaderGainError, MixRoute, SourceControls, SourceId};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::mpsc;
use std::thread;

const MICROPHONE: SourceId = SourceId::new(10);
const APPLICATION: SourceId = SourceId::new(20);

struct CountingAllocator;

thread_local! {
    static COUNT_ALLOCATOR_OPERATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATOR_OPERATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocator_operation();
        // SAFETY: the system allocator receives the unchanged valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocator_operation();
        // SAFETY: the system allocator receives the unchanged valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record_allocator_operation();
        // SAFETY: the pointer and layout came from the system allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocator_operation();
        // SAFETY: the pointer and layout came from the system allocator, and
        // the requested size is forwarded unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

fn record_allocator_operation() {
    if COUNT_ALLOCATOR_OPERATIONS.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATOR_OPERATION_COUNT.try_with(|count| count.set(count.get() + 1));
    }
}

fn count_allocator_operations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATOR_OPERATION_COUNT.with(|count| count.set(0));
    COUNT_ALLOCATOR_OPERATIONS.with(|enabled| enabled.set(true));
    let result = operation();
    COUNT_ALLOCATOR_OPERATIONS.with(|enabled| enabled.set(false));
    let count = ALLOCATOR_OPERATION_COUNT.with(Cell::get);
    (result, count)
}

fn fader(decibels: f32) -> FaderGain {
    match FaderGain::from_decibels(decibels) {
        Ok(fader) => fader,
        Err(error) => panic!("test fader {decibels} dB is invalid: {error}"),
    }
}

fn route(enabled: bool, decibels: f32) -> MixRoute {
    MixRoute::new(enabled, fader(decibels))
}

fn controls(
    source: SourceId,
    monitor_enabled: bool,
    monitor_db: f32,
    stream_enabled: bool,
    stream_db: f32,
) -> SourceControls {
    SourceControls::new(
        source,
        route(monitor_enabled, monitor_db),
        route(stream_enabled, stream_db),
    )
}

fn config(max_frames: usize) -> MixerConfig {
    match MixerConfig::try_new(max_frames, MICROPHONE, &[MICROPHONE, APPLICATION]) {
        Ok(config) => config,
        Err(error) => panic!("test mixer config is invalid: {error}"),
    }
}

fn engine_with(initial: &[SourceControls], max_frames: usize) -> (MixerEngine, ControlStager) {
    match MixerEngine::new(config(max_frames), initial) {
        Ok(engine) => engine,
        Err(error) => panic!("test controls are invalid: {error}"),
    }
}

fn default_controls() -> [SourceControls; 2] {
    [controls(MICROPHONE, true, 0.0, true, 0.0), controls(APPLICATION, true, 0.0, true, 0.0)]
}

fn run_two(
    engine: &mut MixerEngine,
    frames: usize,
    microphone: &[f32],
    application: &[f32],
    input_order: [SourceId; 2],
    output_order: [EndpointId; 3],
) -> (Result<ProcessReport, ProcessError>, [Vec<f32>; 3]) {
    let samples = |source| if source == MICROPHONE { microphone } else { application };
    let inputs = [
        InputBuffer::new(input_order[0], samples(input_order[0])),
        InputBuffer::new(input_order[1], samples(input_order[1])),
    ];
    let sample_count = frames.saturating_mul(CHANNELS);
    let mut data = [vec![99.0; sample_count], vec![99.0; sample_count], vec![99.0; sample_count]];
    let [first, second, third] = &mut data;
    let mut outputs = [
        OutputBuffer::new(output_order[0], first),
        OutputBuffer::new(output_order[1], second),
        OutputBuffer::new(output_order[2], third),
    ];
    let result = engine.process(frames, &inputs, &mut outputs);
    (result, data)
}

#[allow(clippy::large_types_passed_by_value)]
fn report(result: Result<ProcessReport, ProcessError>) -> ProcessReport {
    match result {
        Ok(report) => report,
        Err(error) => panic!("processing failed: {error}"),
    }
}

fn output(data: &[Vec<f32>; 3], order: [EndpointId; 3], endpoint: EndpointId) -> &[f32] {
    match order.iter().position(|candidate| *candidate == endpoint) {
        Some(index) => &data[index],
        None => panic!("test output order omitted an endpoint"),
    }
}

fn assert_close(actual: f32, expected: f32) {
    assert!((actual - expected).abs() <= 1.0e-6, "expected {expected}, received {actual}");
}

#[test]
fn format_and_config_define_a_bounded_portable_layout() {
    assert_eq!(MAX_SOURCES, 12);
    assert_eq!(MIXER_FORMAT.sample_representation, SampleRepresentation::Float32);
    assert_eq!(MIXER_FORMAT.sample_rate_hz, 48_000);
    assert_eq!(MIXER_FORMAT.channels, 2);
    assert_eq!(MIXER_FORMAT.layout, SampleLayout::Interleaved);

    let config = config(1_024);
    assert_eq!(config.max_frames(), 1_024);
    assert_eq!(config.microphone_source(), MICROPHONE);
    assert_eq!(config.sources(), &[MICROPHONE, APPLICATION]);
    assert_eq!(config.format(), MIXER_FORMAT);

    assert_eq!(MixerConfig::try_new(0, MICROPHONE, &[MICROPHONE]), Err(ConfigError::ZeroMaxFrames));
    assert_eq!(
        MixerConfig::try_new(usize::MAX, MICROPHONE, &[MICROPHONE]),
        Err(ConfigError::FrameSampleCountOverflow)
    );
    assert_eq!(
        MixerConfig::try_new(1, MICROPHONE, &[]),
        Err(ConfigError::SourceCount { actual: 0 })
    );
    let too_many = [SourceId::new(1); MAX_SOURCES + 1];
    assert_eq!(
        MixerConfig::try_new(1, SourceId::new(1), &too_many),
        Err(ConfigError::SourceCount { actual: MAX_SOURCES + 1 })
    );
    assert_eq!(
        MixerConfig::try_new(1, MICROPHONE, &[MICROPHONE, MICROPHONE]),
        Err(ConfigError::DuplicateSource(MICROPHONE))
    );
    assert_eq!(
        MixerConfig::try_new(1, MICROPHONE, &[APPLICATION]),
        Err(ConfigError::MicrophoneSourceMissing(MICROPHONE))
    );
}

#[test]
fn fader_gain_rejects_invalid_ranges_steps_and_nonfinite_values() {
    assert_eq!(FaderGain::from_decibels(-60.0), Ok(FaderGain::MIN));
    assert_eq!(FaderGain::from_decibels(0.0), Ok(FaderGain::UNITY));
    assert_eq!(FaderGain::from_decibels(12.0), Ok(FaderGain::MAX));
    assert_eq!(fader(-59.5).half_decibel_steps(), -119);
    assert_eq!(fader(-59.5).decibels(), -59.5);
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(FaderGain::from_decibels(value), Err(FaderGainError::NotFinite));
    }
    for value in [-60.5, 12.5, f32::MAX, -f32::MAX] {
        assert_eq!(FaderGain::from_decibels(value), Err(FaderGainError::OutOfRange));
    }
    assert_eq!(FaderGain::from_decibels(-59.75), Err(FaderGainError::NotHalfDecibelStep));
    assert_eq!(FaderGain::from_half_decibel_steps(-121), Err(FaderGainError::OutOfRange));
    assert_eq!(FaderGain::from_half_decibel_steps(25), Err(FaderGainError::OutOfRange));
}

#[test]
fn controls_require_each_configured_source_exactly_once() {
    let config = config(8);
    assert_eq!(
        MixerEngine::new(config, &[default_controls()[0]]).map(|_| ()),
        Err(ControlMappingError::ControlCount { expected: 2, actual: 1 })
    );
    let unknown = SourceId::new(99);
    assert_eq!(
        MixerEngine::new(
            config,
            &[default_controls()[0], controls(unknown, true, 0.0, true, 0.0),],
        )
        .map(|_| ()),
        Err(ControlMappingError::UnknownSource(unknown))
    );
    assert_eq!(
        MixerEngine::new(config, &[default_controls()[0], default_controls()[0]]).map(|_| ()),
        Err(ControlMappingError::DuplicateSource(MICROPHONE))
    );
}

#[test]
fn explicit_id_mapping_is_order_independent_and_mixes_are_independent() {
    let initial = [
        controls(MICROPHONE, true, 0.0, false, 12.0),
        controls(APPLICATION, false, 12.0, true, 0.0),
    ];
    let (mut engine, _stager) = engine_with(&initial, 4);
    let microphone = [0.25, -0.5, 0.75, -1.0];
    let application = [0.5, 0.25, -0.25, -0.5];
    let order = [EndpointId::StreamMix, EndpointId::Microphone, EndpointId::MonitorMix];
    let (result, data) =
        run_two(&mut engine, 2, &microphone, &application, [APPLICATION, MICROPHONE], order);
    let report = report(result);
    assert!(!report.control_update_applied());
    assert_eq!(output(&data, order, EndpointId::Microphone), microphone);
    assert_eq!(output(&data, order, EndpointId::MonitorMix), microphone);
    assert_eq!(output(&data, order, EndpointId::StreamMix), application);
}

#[test]
fn routes_cover_enable_and_fader_boundaries_and_outputs_clip() {
    let initial = [
        controls(MICROPHONE, true, -60.0, false, 12.0),
        controls(APPLICATION, false, 12.0, true, 12.0),
    ];
    let (mut engine, _stager) = engine_with(&initial, 1);
    let microphone = [2.0, -2.0];
    let application = [0.5, -0.5];
    let order = [EndpointId::Microphone, EndpointId::MonitorMix, EndpointId::StreamMix];
    let (result, data) =
        run_two(&mut engine, 1, &microphone, &application, [MICROPHONE, APPLICATION], order);
    report(result);
    assert_eq!(output(&data, order, EndpointId::Microphone), &[1.0, -1.0]);
    assert_close(output(&data, order, EndpointId::MonitorMix)[0], 0.002);
    assert_close(output(&data, order, EndpointId::MonitorMix)[1], -0.002);
    assert_eq!(output(&data, order, EndpointId::StreamMix), &[1.0, -1.0]);
}

#[test]
fn stereo_peak_and_rms_meters_use_known_frames() {
    let initial = [
        controls(MICROPHONE, true, 0.0, false, 0.0),
        controls(APPLICATION, false, 0.0, false, 0.0),
    ];
    let (mut engine, _stager) = engine_with(&initial, 2);
    let microphone = [1.0, 0.5, -1.0, -0.5];
    let application = [0.0; 4];
    let order = [EndpointId::MonitorMix, EndpointId::StreamMix, EndpointId::Microphone];
    let (result, _) =
        run_two(&mut engine, 2, &microphone, &application, [MICROPHONE, APPLICATION], order);
    let meters = report(result).meters();
    let Some(source) = meters.source(MICROPHONE) else {
        panic!("microphone source meter is missing");
    };
    assert_eq!(source.left().peak(), 1.0);
    assert_eq!(source.left().rms(), 1.0);
    assert_eq!(source.right().peak(), 0.5);
    assert_eq!(source.right().rms(), 0.5);
    assert_eq!(meters.endpoint(EndpointId::Microphone), Some(source));
    assert_eq!(meters.endpoint(EndpointId::MonitorMix), Some(source));
    assert_eq!(meters.endpoint(EndpointId::StreamMix), Some(StereoMeter::ZERO));
    assert_eq!(meters.endpoint(EndpointId::System), None);
    assert_eq!(meters.source(SourceId::new(999)), None);
}

#[test]
fn large_opposing_finite_samples_remain_finite_and_deterministic() {
    let initial = [
        controls(MICROPHONE, true, 12.0, true, 12.0),
        controls(APPLICATION, true, 12.0, true, 12.0),
    ];
    let (mut engine, _stager) = engine_with(&initial, 1);
    let microphone = [f32::MAX, -f32::MAX];
    let application = [-f32::MAX, f32::MAX];
    let order = [EndpointId::MonitorMix, EndpointId::Microphone, EndpointId::StreamMix];
    let (first_result, first) =
        run_two(&mut engine, 1, &microphone, &application, [MICROPHONE, APPLICATION], order);
    let first_report = report(first_result);
    let (second_result, second) =
        run_two(&mut engine, 1, &microphone, &application, [APPLICATION, MICROPHONE], order);
    let second_report = report(second_result);
    assert_eq!(first, second);
    assert_eq!(first_report.meters(), second_report.meters());
    assert_eq!(output(&first, order, EndpointId::MonitorMix), &[0.0, 0.0]);
    assert_eq!(output(&first, order, EndpointId::StreamMix), &[0.0, 0.0]);
    assert_eq!(output(&first, order, EndpointId::Microphone), &[1.0, -1.0]);
    let Some(source) = first_report.meters().source(MICROPHONE) else {
        panic!("microphone source meter is missing");
    };
    assert!(source.left().peak().is_finite());
    assert!(source.left().rms().is_finite());
    assert_eq!(source.left().peak(), f32::MAX);
}

#[test]
fn zero_frames_return_zero_meters_and_preserve_a_pending_update() {
    let initial = [
        controls(MICROPHONE, false, 0.0, false, 0.0),
        controls(APPLICATION, false, 0.0, false, 0.0),
    ];
    let next = [
        controls(MICROPHONE, true, 0.0, false, 0.0),
        controls(APPLICATION, false, 0.0, false, 0.0),
    ];
    let (mut engine, mut stager) = engine_with(&initial, 2);
    assert_eq!(stager.try_stage(&next), Ok(()));
    let order = [EndpointId::Microphone, EndpointId::MonitorMix, EndpointId::StreamMix];
    let (zero_result, zero_outputs) =
        run_two(&mut engine, 0, &[], &[], [MICROPHONE, APPLICATION], order);
    let zero = report(zero_result);
    assert!(!zero.control_update_applied());
    assert_eq!(zero.meters().endpoint(EndpointId::Microphone), Some(StereoMeter::ZERO));
    assert_eq!(zero.meters().endpoint(EndpointId::MonitorMix), Some(StereoMeter::ZERO));
    assert!(zero_outputs.iter().all(Vec::is_empty));
    assert_eq!(stager.try_stage(&initial), Err(StageError::Full));

    let (result, data) =
        run_two(&mut engine, 1, &[0.5, -0.5], &[0.0, 0.0], [MICROPHONE, APPLICATION], order);
    assert!(report(result).control_update_applied());
    assert_eq!(output(&data, order, EndpointId::MonitorMix), &[0.5, -0.5]);
}

#[test]
fn rejected_block_preserves_outputs_active_controls_and_pending_update() {
    let initial = [
        controls(MICROPHONE, false, 0.0, false, 0.0),
        controls(APPLICATION, false, 0.0, false, 0.0),
    ];
    let next = [
        controls(MICROPHONE, true, 0.0, false, 0.0),
        controls(APPLICATION, false, 0.0, false, 0.0),
    ];
    let (mut engine, mut stager) = engine_with(&initial, 1);
    assert_eq!(stager.try_stage(&next), Ok(()));
    let silent = [0.0; 2];
    for invalid_sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let invalid = [invalid_sample, 0.0];
        let inputs =
            [InputBuffer::new(MICROPHONE, &invalid), InputBuffer::new(APPLICATION, &silent)];
        let mut microphone = [7.0; 2];
        let mut monitor = [7.0; 2];
        let mut stream = [7.0; 2];
        let mut outputs = [
            OutputBuffer::new(EndpointId::Microphone, &mut microphone),
            OutputBuffer::new(EndpointId::MonitorMix, &mut monitor),
            OutputBuffer::new(EndpointId::StreamMix, &mut stream),
        ];
        assert_eq!(
            engine.process(1, &inputs, &mut outputs),
            Err(ProcessError::NonFiniteInput { source: MICROPHONE, sample_index: 0 })
        );
        assert_eq!(microphone, [7.0; 2]);
        assert_eq!(monitor, [7.0; 2]);
        assert_eq!(stream, [7.0; 2]);
    }
    assert_eq!(stager.try_stage(&initial), Err(StageError::Full));

    let order = [EndpointId::Microphone, EndpointId::MonitorMix, EndpointId::StreamMix];
    let (result, data) =
        run_two(&mut engine, 1, &[0.25, -0.25], &silent, [MICROPHONE, APPLICATION], order);
    assert!(report(result).control_update_applied());
    assert_eq!(output(&data, order, EndpointId::MonitorMix), &[0.25, -0.25]);
}

#[test]
fn frame_and_input_buffer_validation_is_exact() {
    let (mut engine, _stager) = engine_with(&default_controls(), 2);
    let empty_inputs: [InputBuffer<'_>; 0] = [];
    let mut empty_outputs: [OutputBuffer<'_>; 0] = [];
    assert_eq!(
        engine.process(usize::MAX, &empty_inputs, &mut empty_outputs),
        Err(ProcessError::FrameSampleCountOverflow { actual_frames: usize::MAX })
    );
    assert_eq!(
        engine.process(3, &empty_inputs, &mut empty_outputs),
        Err(ProcessError::FrameCountExceedsMaximum { maximum: 2, actual: 3 })
    );

    let samples = [0.0; 4];
    let short = [0.0; 3];
    let long = [0.0; 5];
    let unknown = SourceId::new(99);
    let mut microphone = [0.0; 4];
    let mut monitor = [0.0; 4];
    let mut stream = [0.0; 4];
    let mut valid_outputs = [
        OutputBuffer::new(EndpointId::Microphone, &mut microphone),
        OutputBuffer::new(EndpointId::MonitorMix, &mut monitor),
        OutputBuffer::new(EndpointId::StreamMix, &mut stream),
    ];
    assert_eq!(
        engine.process(2, &[InputBuffer::new(MICROPHONE, &samples)], &mut valid_outputs),
        Err(ProcessError::InputCount { expected: 2, actual: 1 })
    );
    assert_eq!(
        engine.process(
            2,
            &[
                InputBuffer::new(MICROPHONE, &samples),
                InputBuffer::new(APPLICATION, &samples),
                InputBuffer::new(unknown, &samples),
            ],
            &mut valid_outputs,
        ),
        Err(ProcessError::InputCount { expected: 2, actual: 3 })
    );
    assert_eq!(
        engine.process(
            2,
            &[InputBuffer::new(MICROPHONE, &samples), InputBuffer::new(unknown, &samples),],
            &mut valid_outputs,
        ),
        Err(ProcessError::UnknownInput(unknown))
    );
    assert_eq!(
        engine.process(
            2,
            &[InputBuffer::new(MICROPHONE, &samples), InputBuffer::new(MICROPHONE, &samples),],
            &mut valid_outputs,
        ),
        Err(ProcessError::DuplicateInput(MICROPHONE))
    );
    for (input, actual) in [(&short[..], 3), (&long[..], 5)] {
        assert_eq!(
            engine.process(
                2,
                &[InputBuffer::new(MICROPHONE, input), InputBuffer::new(APPLICATION, &samples),],
                &mut valid_outputs,
            ),
            Err(ProcessError::InputLength { source: MICROPHONE, expected: 4, actual })
        );
    }
}

#[test]
fn output_buffer_validation_is_exact() {
    let (mut engine, _stager) = engine_with(&default_controls(), 2);
    let samples = [0.0; 4];
    let valid_inputs =
        [InputBuffer::new(MICROPHONE, &samples), InputBuffer::new(APPLICATION, &samples)];
    let mut first = [0.0; 4];
    let mut second = [0.0; 4];
    let mut only_two = [
        OutputBuffer::new(EndpointId::Microphone, &mut first),
        OutputBuffer::new(EndpointId::MonitorMix, &mut second),
    ];
    assert_eq!(
        engine.process(2, &valid_inputs, &mut only_two),
        Err(ProcessError::OutputCount { expected: 3, actual: 2 })
    );
    let mut first = [0.0; 4];
    let mut second = [0.0; 4];
    let mut third = [0.0; 4];
    let mut fourth = [0.0; 4];
    let mut four = [
        OutputBuffer::new(EndpointId::Microphone, &mut first),
        OutputBuffer::new(EndpointId::MonitorMix, &mut second),
        OutputBuffer::new(EndpointId::StreamMix, &mut third),
        OutputBuffer::new(EndpointId::StreamMix, &mut fourth),
    ];
    assert_eq!(
        engine.process(2, &valid_inputs, &mut four),
        Err(ProcessError::OutputCount { expected: 3, actual: 4 })
    );
    let mut first = [0.0; 4];
    let mut second = [0.0; 4];
    let mut third = [0.0; 4];
    let mut duplicate = [
        OutputBuffer::new(EndpointId::Microphone, &mut first),
        OutputBuffer::new(EndpointId::Microphone, &mut second),
        OutputBuffer::new(EndpointId::StreamMix, &mut third),
    ];
    assert_eq!(
        engine.process(2, &valid_inputs, &mut duplicate),
        Err(ProcessError::DuplicateOutput(EndpointId::Microphone))
    );
    let mut first = [0.0; 4];
    for actual in [3, 5] {
        let mut second = vec![0.0; actual];
        let mut third = [0.0; 4];
        let mut wrong_length = [
            OutputBuffer::new(EndpointId::Microphone, &mut first),
            OutputBuffer::new(EndpointId::MonitorMix, &mut second),
            OutputBuffer::new(EndpointId::StreamMix, &mut third),
        ];
        assert_eq!(
            engine.process(2, &valid_inputs, &mut wrong_length),
            Err(ProcessError::OutputLength {
                endpoint: EndpointId::MonitorMix,
                expected: 4,
                actual,
            })
        );
    }
}

#[test]
fn system_sink_is_rejected_as_an_output_before_mutation() {
    let (mut engine, _stager) = engine_with(&default_controls(), 1);
    let samples = [0.0; 2];
    let inputs = [InputBuffer::new(MICROPHONE, &samples), InputBuffer::new(APPLICATION, &samples)];
    let mut system = [7.0; 2];
    let mut monitor = [7.0; 2];
    let mut stream = [7.0; 2];
    let mut outputs = [
        OutputBuffer::new(EndpointId::System, &mut system),
        OutputBuffer::new(EndpointId::MonitorMix, &mut monitor),
        OutputBuffer::new(EndpointId::StreamMix, &mut stream),
    ];
    assert_eq!(
        engine.process(1, &inputs, &mut outputs),
        Err(ProcessError::EndpointIsNotMixerOutput(EndpointId::System))
    );
    assert_eq!(system, [7.0; 2]);
    assert_eq!(monitor, [7.0; 2]);
    assert_eq!(stream, [7.0; 2]);
}

#[test]
fn handoff_is_bounded_and_applies_complete_snapshots_at_block_boundaries() {
    let initial = [
        controls(MICROPHONE, false, 0.0, false, 0.0),
        controls(APPLICATION, false, 0.0, false, 0.0),
    ];
    let first =
        [controls(APPLICATION, false, 0.0, true, 0.0), controls(MICROPHONE, true, 0.0, false, 0.0)];
    let second =
        [controls(MICROPHONE, false, 0.0, true, 0.0), controls(APPLICATION, true, 0.0, false, 0.0)];
    let (mut engine, mut stager) = engine_with(&initial, 1);
    assert_eq!(stager.try_stage(&first), Ok(()));
    assert_eq!(stager.try_stage(&second), Err(StageError::Full));
    let order = [EndpointId::MonitorMix, EndpointId::StreamMix, EndpointId::Microphone];
    let (result, data) =
        run_two(&mut engine, 1, &[0.25, -0.25], &[0.5, -0.5], [MICROPHONE, APPLICATION], order);
    assert!(report(result).control_update_applied());
    assert_eq!(output(&data, order, EndpointId::MonitorMix), &[0.25, -0.25]);
    assert_eq!(output(&data, order, EndpointId::StreamMix), &[0.5, -0.5]);
    assert_eq!(stager.try_stage(&second), Ok(()));
    let (result, data) =
        run_two(&mut engine, 1, &[0.25, -0.25], &[0.5, -0.5], [APPLICATION, MICROPHONE], order);
    assert!(report(result).control_update_applied());
    assert_eq!(output(&data, order, EndpointId::MonitorMix), &[0.5, -0.5]);
    assert_eq!(output(&data, order, EndpointId::StreamMix), &[0.25, -0.25]);
}

#[test]
fn spsc_handoff_publishes_coherent_snapshots_between_threads() {
    let initial = [
        controls(MICROPHONE, false, 0.0, false, 0.0),
        controls(APPLICATION, false, 0.0, false, 0.0),
    ];
    let (mut engine, mut stager) = engine_with(&initial, 1);
    let (ready_sender, ready_receiver) = mpsc::sync_channel::<bool>(0);
    let (result_sender, result_receiver) = mpsc::sync_channel::<[f32; 2]>(0);
    let audio = thread::spawn(move || {
        while let Ok(microphone_enabled) = ready_receiver.recv() {
            let order = [EndpointId::MonitorMix, EndpointId::Microphone, EndpointId::StreamMix];
            let (result, data) = run_two(
                &mut engine,
                1,
                &[0.25, -0.25],
                &[0.5, -0.5],
                [APPLICATION, MICROPHONE],
                order,
            );
            assert!(report(result).control_update_applied());
            let monitor = output(&data, order, EndpointId::MonitorMix);
            let actual = [monitor[0], monitor[1]];
            let expected = if microphone_enabled { [0.25, -0.25] } else { [0.5, -0.5] };
            assert_eq!(actual, expected);
            if result_sender.send(actual).is_err() {
                return;
            }
        }
    });

    for iteration in 0..64 {
        let microphone_enabled = iteration % 2 == 0;
        let snapshot = [
            controls(MICROPHONE, microphone_enabled, 0.0, false, 0.0),
            controls(APPLICATION, !microphone_enabled, 0.0, false, 0.0),
        ];
        assert_eq!(stager.try_stage(&snapshot), Ok(()));
        assert!(ready_sender.send(microphone_enabled).is_ok());
        assert!(result_receiver.recv().is_ok());
    }
    drop(ready_sender);
    assert!(audio.join().is_ok());
}

#[test]
fn repeated_processing_is_deterministic_and_has_no_allocator_operations() {
    let (mut engine, _stager) = engine_with(&default_controls(), 8);
    let microphone = [0.125_f32; 16];
    let application = [-0.0625_f32; 16];
    let inputs =
        [InputBuffer::new(MICROPHONE, &microphone), InputBuffer::new(APPLICATION, &application)];
    let mut microphone_output = [0.0_f32; 16];
    let mut monitor_output = [0.0_f32; 16];
    let mut stream_output = [0.0_f32; 16];

    {
        let mut outputs = [
            OutputBuffer::new(EndpointId::Microphone, &mut microphone_output),
            OutputBuffer::new(EndpointId::MonitorMix, &mut monitor_output),
            OutputBuffer::new(EndpointId::StreamMix, &mut stream_output),
        ];
        report(engine.process(8, &inputs, &mut outputs));
    }
    let expected_microphone = microphone_output;
    let expected_monitor = monitor_output;
    let expected_stream = stream_output;

    let (checksum, allocator_operations) = count_allocator_operations(|| {
        let mut checksum = 0.0_f32;
        for _ in 0..1_000 {
            let mut outputs = [
                OutputBuffer::new(EndpointId::StreamMix, &mut stream_output),
                OutputBuffer::new(EndpointId::Microphone, &mut microphone_output),
                OutputBuffer::new(EndpointId::MonitorMix, &mut monitor_output),
            ];
            let current = report(engine.process(8, &inputs, &mut outputs));
            checksum += current
                .meters()
                .endpoint(EndpointId::MonitorMix)
                .expect("monitor output meter")
                .left()
                .rms();
        }
        checksum
    });
    assert_eq!(allocator_operations, 0);
    assert!(checksum > 0.0);
    assert_eq!(microphone_output, expected_microphone);
    assert_eq!(monitor_output, expected_monitor);
    assert_eq!(stream_output, expected_stream);
}

#[test]
fn meter_handoff_is_empty_until_a_real_block_and_never_overwrites() {
    let (mut publisher, mut reader) = MeterPublisher::channel();
    assert_eq!(reader.try_take(), None);

    let (mut engine, _stager) = engine_with(&default_controls(), 1);
    let order = [EndpointId::Microphone, EndpointId::MonitorMix, EndpointId::StreamMix];
    let (result, _) =
        run_two(&mut engine, 1, &[0.25, -0.5], &[0.0, 0.0], [MICROPHONE, APPLICATION], order);
    let meters = report(result).meters();
    assert!(publisher.try_publish(meters));
    assert!(!publisher.try_publish(meters));
    assert_eq!(reader.try_take(), Some(meters));
    assert_eq!(reader.try_take(), None);
}

fn rate_match_bounds() -> RateMatchRatioBounds {
    match RateMatchRatioBounds::try_new(2.0, 0.99, 1.01) {
        Ok(bounds) => bounds,
        Err(error) => panic!("test rate bounds are invalid: {error}"),
    }
}

fn rate_match_config(channels: usize) -> RateMatcherConfig {
    match RateMatcherConfig::try_new(1_024, channels, rate_match_bounds()) {
        Ok(config) => config,
        Err(error) => panic!("test rate matcher config is invalid: {error}"),
    }
}

fn rate_match_controller_config() -> RateMatchControllerConfig {
    match RateMatchControllerConfig::try_new(100, 0.005, 0.001, 1.0, 0.01, rate_match_bounds()) {
        Ok(config) => config,
        Err(error) => panic!("test rate controller config is invalid: {error}"),
    }
}

#[test]
fn rate_match_bounds_and_resource_shape_are_checked_before_construction() {
    assert!(RateMatchRatioBounds::try_new(0.99, 0.5, 2.0).is_err());
    assert!(RateMatchRatioBounds::try_new(2.01, 0.5, 2.0).is_err());
    assert!(RateMatchRatioBounds::try_new(2.0, 0.49, 2.0).is_err());
    assert!(RateMatchRatioBounds::try_new(2.0, 0.5, 2.01).is_err());
    assert!(RateMatchRatioBounds::try_new(2.0, 1.1, 1.0).is_err());
    assert!(RateMatchRatioBounds::try_new(1.1, 0.5, 2.0).is_err());

    assert!(RateMatcherConfig::try_new(0, 1, rate_match_bounds()).is_err());
    assert!(RateMatcherConfig::try_new(1_025, 1, rate_match_bounds()).is_err());
    assert!(RateMatcherConfig::try_new(1_024, 0, rate_match_bounds()).is_err());
    assert!(RateMatcherConfig::try_new(1_024, 3, rate_match_bounds()).is_err());
    assert!(RateMatcherConfig::try_new(1_024, usize::MAX, rate_match_bounds()).is_err());
    assert_eq!(rate_match_config(1).channels(), 1);
    assert_eq!(rate_match_config(2).channels(), 2);
    assert_eq!(rate_match_controller_config().target_fill_frames(), 100);
    assert_eq!(rate_match_controller_config().ratio_bounds(), rate_match_bounds());

    let matcher = RateMatcher::new(rate_match_config(2)).expect("test matcher construction");
    assert!(matcher.maximum_input_frames() >= matcher.config().max_output_frames());
    assert!(matcher.output_delay_frames() > 0);
}

#[test]
fn rate_match_controller_preview_commit_and_reset_are_transactional() {
    let mut controller = RateMatchController::new(rate_match_controller_config());
    let initial = (controller.integral(), controller.ratio());
    {
        let preview = controller.preview(200).expect("preview arithmetic should succeed");
        assert!(preview.relative_ratio() < 1.0);
        assert_eq!((controller.integral(), controller.ratio()), initial);
    }
    assert_eq!((controller.integral(), controller.ratio()), initial);

    let ratio = {
        let step = controller.preview(0).expect("preview arithmetic should succeed");
        let ratio = step.relative_ratio();
        step.commit();
        ratio
    };
    assert!(ratio > 1.0);
    assert_eq!(controller.ratio(), ratio);

    controller.reset();
    controller.preview(0).expect("preview arithmetic should succeed").commit();
    assert_eq!(controller.integral(), 1.0);
    controller.preview(0).expect("preview arithmetic should succeed").commit();
    assert_eq!(controller.integral(), 1.0, "integral must clamp at its configured limit");

    controller.reset();
    controller.preview(200).expect("preview arithmetic should succeed").commit();
    assert_eq!(controller.integral(), -1.0);
}

#[test]
fn extreme_finite_controller_arithmetic_is_rejected_without_mutation() {
    let config = match RateMatchControllerConfig::try_new(
        100,
        f64::MAX,
        f64::MAX,
        f64::MAX,
        f64::MAX,
        rate_match_bounds(),
    ) {
        Ok(config) => config,
        Err(error) => panic!("extreme test controller config is invalid: {error}"),
    };
    let mut controller = RateMatchController::new(config);
    let initial = (controller.integral(), controller.ratio());
    let error =
        controller.preview(0).expect_err("finite extreme arithmetic must not create a token");
    assert!(matches!(
        error,
        RateMatchControllerError::NonFinitePreview {
            stage: RateMatchControllerArithmetic::DesiredSum
        }
    ));
    assert_eq!((controller.integral(), controller.ratio()), initial);
}

#[test]
fn rate_matcher_requires_real_warmup_and_failed_cycles_need_reset() {
    let mut matcher = match RateMatcher::new(rate_match_config(1)) {
        Ok(matcher) => matcher,
        Err(error) => panic!("test matcher construction failed: {error}"),
    };
    let mut output = vec![7.0_f32; 64];
    assert!(!matcher.is_ready());
    assert!(matches!(matcher.prepare(64, 1.0, &mut output), Err(RateMatchError::NotWarmed)));
    matcher.warm().expect("zero-input warmup should succeed");
    assert!(matcher.is_ready());

    let mut controller = RateMatchController::new(rate_match_controller_config());
    let initial = (controller.integral(), controller.ratio());
    let mut output = vec![7.0_f32; 64];
    {
        let step = controller.preview(100).expect("preview arithmetic should succeed");
        let cycle =
            matcher.prepare(64, step.relative_ratio(), &mut output).expect("cycle should prepare");
        drop(cycle);
    }
    assert_eq!((controller.integral(), controller.ratio()), initial);
    assert!(matcher.needs_reset());
    assert!(!matcher.is_ready());
    assert!(matches!(matcher.prepare(64, 1.0, &mut output), Err(RateMatchError::NeedsReset)));

    matcher.reset();
    matcher.warm().expect("reset matcher should warm again");
    let mut output = vec![7.0_f32; 64];
    let cycle = matcher.prepare(64, 1.0, &mut output).expect("cycle should prepare after warmup");
    let short_input = vec![0.0_f32; cycle.input_frames() - 1];
    let error = cycle.process(&short_input).expect_err("short input must be terminal");
    assert!(error.is_terminal());
    assert!(matcher.needs_reset());
    assert!(output.iter().all(|sample| *sample == 7.0));
}

fn process_rate_matcher(channels: usize, quantum: usize) -> RateMatchReport {
    let mut matcher = match RateMatcher::new(rate_match_config(channels)) {
        Ok(matcher) => matcher,
        Err(error) => panic!("test matcher construction failed: {error}"),
    };
    matcher.warm().expect("zero-input warmup should succeed");
    let mut output = vec![0.0_f32; quantum * channels];
    let cycle =
        matcher.prepare(quantum, 0.99, &mut output).expect("configured quantum should prepare");
    let input = vec![0.125_f32; cycle.input_frames() * channels];
    let report = cycle.process(&input).expect("complete finite cycle should process");
    assert_eq!(report.output_frames(), quantum);
    assert_eq!(report.input_frames(), input.len() / channels);
    assert!(report.relative_ratio().is_finite());
    assert!(output.iter().all(|sample| sample.is_finite()));
    report
}

#[test]
fn rate_matcher_supports_mono_capture_and_stereo_playback_shapes() {
    for quantum in [64, 65, 512, 1_024] {
        let mono = process_rate_matcher(1, quantum);
        let stereo = process_rate_matcher(2, quantum);
        assert_eq!(mono.output_frames(), quantum);
        assert_eq!(stereo.output_frames(), quantum);
    }
}

#[test]
fn rate_matcher_supports_the_full_configured_quantum_range() {
    let mut matcher = match RateMatcher::new(rate_match_config(1)) {
        Ok(matcher) => matcher,
        Err(error) => panic!("test matcher construction failed: {error}"),
    };
    matcher.warm().expect("zero-input warmup should succeed");
    let mut input = vec![0.125_f32; 4_096];
    let mut output = vec![0.0_f32; 1_024];

    for quantum in 1..=1_024 {
        let ratio = if quantum % 2 == 0 { 0.99 } else { 1.01 };
        let cycle = matcher
            .prepare(quantum, ratio, &mut output[..quantum])
            .expect("every configured quantum should prepare");
        let input_frames = cycle.input_frames();
        let report =
            cycle.process(&input[..input_frames]).expect("every configured quantum should process");
        assert_eq!(report.input_frames(), input_frames);
        assert_eq!(report.output_frames(), quantum);
        assert!(output[..quantum].iter().all(|sample| sample.is_finite()));
        input[..input_frames].fill(0.125);
    }
}

#[test]
fn rate_matcher_accepts_dependency_ratio_endpoints_with_exact_counts() {
    let bounds = match RateMatchRatioBounds::try_new(2.0, 0.5, 2.0) {
        Ok(bounds) => bounds,
        Err(error) => panic!("dependency endpoint bounds are invalid: {error}"),
    };
    for channels in [1, 2] {
        let config = match RateMatcherConfig::try_new(1_024, channels, bounds) {
            Ok(config) => config,
            Err(error) => panic!("endpoint matcher config is invalid: {error}"),
        };
        let mut matcher = match RateMatcher::new(config) {
            Ok(matcher) => matcher,
            Err(error) => panic!("endpoint matcher construction failed: {error}"),
        };
        matcher.warm().expect("zero-input warmup should succeed");
        let mut input = vec![0.125_f32; 4_096 * channels];
        let mut output = vec![0.0_f32; 64 * channels];
        for ratio in [bounds.minimum(), bounds.maximum()] {
            let cycle = matcher
                .prepare(64, ratio, &mut output)
                .expect("dependency ratio endpoint should prepare");
            let input_frames = cycle.input_frames();
            let report = cycle
                .process(&input[..input_frames * channels])
                .expect("dependency ratio endpoint should process");
            assert_eq!(report.input_frames(), input_frames);
            assert_eq!(report.output_frames(), 64);
            assert!(output.iter().all(|sample| sample.is_finite()));
            input[..input_frames * channels].fill(0.125);
        }
    }
}

#[test]
fn non_finite_rate_match_input_is_terminal_after_prepare() {
    let mut matcher = match RateMatcher::new(rate_match_config(2)) {
        Ok(matcher) => matcher,
        Err(error) => panic!("test matcher construction failed: {error}"),
    };
    matcher.warm().expect("zero-input warmup should succeed");
    let mut output = vec![3.0_f32; 64 * 2];
    let cycle = matcher.prepare(64, 1.0, &mut output).expect("cycle should prepare");
    let mut input = vec![0.125_f32; cycle.input_frames() * 2];
    input[1] = f32::NAN;
    let error = cycle.process(&input).expect_err("non-finite input must be rejected");
    assert!(matches!(error, RateMatchError::NonFiniteInput { .. }));
    assert!(error.is_terminal());
    assert!(matcher.needs_reset());
    assert!(output.iter().all(|sample| *sample == 3.0));
}
