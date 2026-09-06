use librewave_engine::{
    RateMatchController, RateMatchControllerConfig, RateMatchControllerStep, RateMatchError,
    RateMatchRatioBounds, RateMatcher, RateMatcherConfig,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;

thread_local! {
    static COUNTING_ENABLED: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static REALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static DEALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(0);
        // SAFETY: the system allocator receives the unchanged valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(0);
        // SAFETY: the system allocator receives the unchanged valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record(2);
        // SAFETY: the pointer and layout came from the system allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(1);
        // SAFETY: the pointer and layout came from the system allocator, and
        // the requested size is forwarded unchanged.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

fn record(kind: u8) {
    if COUNTING_ENABLED.try_with(Cell::get).unwrap_or(false) {
        match kind {
            0 => {
                let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
            }
            1 => {
                let _ = REALLOCATIONS.try_with(|count| count.set(count.get() + 1));
            }
            _ => {
                let _ = DEALLOCATIONS.try_with(|count| count.set(count.get() + 1));
            }
        }
    }
}

fn count_operations<T>(operation: impl FnOnce() -> T) -> (T, (usize, usize, usize)) {
    ALLOCATIONS.with(|count| count.set(0));
    REALLOCATIONS.with(|count| count.set(0));
    DEALLOCATIONS.with(|count| count.set(0));
    COUNTING_ENABLED.with(|enabled| enabled.set(true));
    let result = operation();
    COUNTING_ENABLED.with(|enabled| enabled.set(false));
    let counts =
        (ALLOCATIONS.with(Cell::get), REALLOCATIONS.with(Cell::get), DEALLOCATIONS.with(Cell::get));
    (result, counts)
}

fn assert_no_operations(counts: (usize, usize, usize)) {
    assert_eq!(counts.0, 0, "path must not allocate");
    assert_eq!(counts.1, 0, "path must not reallocate");
    assert_eq!(counts.2, 0, "path must not deallocate");
}

fn bounds() -> RateMatchRatioBounds {
    match RateMatchRatioBounds::try_new(2.0, 0.99, 1.01) {
        Ok(bounds) => bounds,
        Err(error) => panic!("test ratio bounds are invalid: {error}"),
    }
}

fn matcher_config(channels: usize) -> RateMatcherConfig {
    match RateMatcherConfig::try_new(1_024, channels, bounds()) {
        Ok(config) => config,
        Err(error) => panic!("test matcher config is invalid: {error}"),
    }
}

fn controller_config() -> RateMatchControllerConfig {
    match RateMatchControllerConfig::try_new(100, 0.005, 0.001, 1.0, 0.01, bounds()) {
        Ok(config) => config,
        Err(error) => panic!("test controller config is invalid: {error}"),
    }
}

fn preview(
    controller: &mut RateMatchController,
    fill_frames: usize,
) -> RateMatchControllerStep<'_> {
    match controller.preview(fill_frames, 1.0) {
        Ok(step) => step,
        Err(error) => panic!("test controller preview failed: {error}"),
    }
}

fn measure_retryable_output_error(
    matcher: &mut RateMatcher,
    controller: &mut RateMatchController,
    output: &mut [f32],
) -> (usize, usize, usize) {
    let initial_controller_state = (controller.integral(), controller.ratio());
    let ((), counts) = count_operations(|| {
        let preview = preview(controller, 100);
        assert!(matches!(
            matcher.prepare(64, preview.ratio(), &mut output[..1]),
            Err(RateMatchError::OutputLength { .. })
        ));
    });
    assert_eq!((controller.integral(), controller.ratio()), initial_controller_state);
    counts
}

fn measure_dropped_cycle(
    channels: usize,
    matcher: &mut RateMatcher,
    controller: &mut RateMatchController,
    output: &mut [f32],
) -> (usize, usize, usize) {
    let initial_controller_state = (controller.integral(), controller.ratio());
    let ((), counts) = count_operations(|| {
        let preview = preview(controller, 100);
        let cycle = match matcher.prepare(64, preview.ratio(), &mut output[..64 * channels]) {
            Ok(cycle) => cycle,
            Err(error) => panic!("test cycle prepare failed: {error}"),
        };
        drop(cycle);
    });
    assert!(matcher.needs_reset());
    assert_eq!((controller.integral(), controller.ratio()), initial_controller_state);
    counts
}

fn measure_short_cycle(
    channels: usize,
    matcher: &mut RateMatcher,
    controller: &mut RateMatchController,
    input: &[f32],
    output: &mut [f32],
) -> (usize, usize, usize) {
    let initial_controller_state = (controller.integral(), controller.ratio());
    let ((), counts) = count_operations(|| {
        let preview = preview(controller, 100);
        let cycle = match matcher.prepare(64, preview.ratio(), &mut output[..64 * channels]) {
            Ok(cycle) => cycle,
            Err(error) => panic!("test cycle prepare failed: {error}"),
        };
        let short_sample_count = cycle.input_frames() * channels - 1;
        let error =
            cycle.process(&input[..short_sample_count]).expect_err("short input must be terminal");
        assert!(error.is_terminal());
    });
    assert!(matcher.needs_reset());
    assert_eq!((controller.integral(), controller.ratio()), initial_controller_state);
    counts
}

fn run_steady_cycles(
    channels: usize,
    matcher: &mut RateMatcher,
    controller: &mut RateMatchController,
    input: &mut [f32],
    output: &mut [f32],
) -> f32 {
    let mut checksum = 0.0_f32;
    for iteration in 0..2_000 {
        let quantum = match iteration % 4 {
            0 => 64,
            1 => 65,
            2 => 512,
            _ => 1_024,
        };
        let fill = if iteration % 2 == 0 { 90 } else { 110 };
        let preview = preview(controller, fill);
        let ratio = preview.ratio();
        let cycle = match matcher.prepare(quantum, ratio, &mut output[..quantum * channels]) {
            Ok(cycle) => cycle,
            Err(error) => panic!("steady-state cycle prepare failed: {error}"),
        };
        let input_sample_count = cycle.input_frames() * channels;
        input[..input_sample_count].fill(0.125);
        let report = match cycle.process(&input[..input_sample_count]) {
            Ok(report) => report,
            Err(error) => panic!("steady-state cycle process failed: {error}"),
        };
        preview.commit();
        assert_eq!(report.output_frames(), quantum);
        assert_eq!(report.input_frames(), input_sample_count / channels);
        assert!(report.relative_ratio().is_finite());
        for sample in &output[..quantum * channels] {
            assert!(sample.is_finite());
            checksum += *sample;
        }
    }
    checksum
}

fn run_shape_without_realtime_allocator_operations(channels: usize) {
    let mut matcher = match RateMatcher::new(matcher_config(channels)) {
        Ok(matcher) => matcher,
        Err(error) => panic!("test matcher construction failed: {error}"),
    };
    if let Err(error) = matcher.warm() {
        panic!("test matcher warmup failed: {error}");
    }
    let mut controller = RateMatchController::new(controller_config());
    let mut input = vec![0.125_f32; 4_096 * channels];
    let mut output = vec![0.0_f32; 1_024 * channels];

    assert_no_operations(measure_retryable_output_error(
        &mut matcher,
        &mut controller,
        &mut output,
    ));
    assert_no_operations(measure_dropped_cycle(
        channels,
        &mut matcher,
        &mut controller,
        &mut output,
    ));
    matcher.reset();
    if let Err(error) = matcher.warm() {
        panic!("test matcher re-warm after dropped cycle failed: {error}");
    }

    assert_no_operations(measure_short_cycle(
        channels,
        &mut matcher,
        &mut controller,
        &input,
        &mut output,
    ));
    matcher.reset();
    if let Err(error) = matcher.warm() {
        panic!("test matcher re-warm after short cycle failed: {error}");
    }

    let (checksum, steady_counts) = count_operations(|| {
        run_steady_cycles(channels, &mut matcher, &mut controller, &mut input, &mut output)
    });
    assert!(checksum.is_finite());
    assert!(checksum > 0.0);
    assert_no_operations(steady_counts);

    let ((), off_realtime_counts) = count_operations(|| drop(matcher));
    assert_eq!(off_realtime_counts.0, 0, "off-realtime destruction must not allocate");
    assert_eq!(off_realtime_counts.1, 0, "off-realtime destruction must not reallocate");
    assert!(off_realtime_counts.2 > 0, "off-realtime destruction must release scratch");
}

#[test]
fn mono_capture_shaped_cycles_do_not_touch_the_allocator() {
    run_shape_without_realtime_allocator_operations(1);
}

#[test]
fn stereo_playback_shaped_cycles_do_not_touch_the_allocator() {
    run_shape_without_realtime_allocator_operations(2);
}
