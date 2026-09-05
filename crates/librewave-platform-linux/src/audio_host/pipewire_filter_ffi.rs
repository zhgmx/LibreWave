//! Private `pw_filter` ownership and realtime callback boundary.

use pipewire as pw;
use std::cell::UnsafeCell;
use std::ffi::{CString, c_char, c_int, c_void};
use std::num::NonZeroU32;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::ptr::NonNull;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

const PORT_COUNT: usize = 8;
const FAULT_CAPACITY: usize = 16;
const FILTER_STATE_OTHER: i32 = 0;
pub(super) const FILTER_STATE_PAUSED: i32 = 1;
pub(super) const FILTER_STATE_STREAMING: i32 = 2;
const FILTER_STATE_ERROR: i32 = 3;

#[repr(C)]
struct NativeFilter {
    _private: [u8; 0],
}

#[cfg(test)]
#[repr(C)]
struct NativeTestPeer {
    _private: [u8; 0],
}

type NativeProcessCallback =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, u64, u32, *mut *mut f32, u32) -> c_int;
type NativeStateCallback = unsafe extern "C" fn(*mut c_void, c_int, c_int);
#[cfg(test)]
type NativeTestProcessCallback =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, u64, u32, *mut *mut f32, u32);

unsafe extern "C" {
    fn lw_pw_filter_new(
        core: *mut c_void,
        name: *const c_char,
        properties: *mut c_void,
        process: NativeProcessCallback,
        state_changed: NativeStateCallback,
        data: *mut c_void,
    ) -> *mut NativeFilter;
    fn lw_pw_filter_add_port(
        filter: *mut NativeFilter,
        index: u32,
        output: bool,
        properties: *mut c_void,
    ) -> c_int;
    fn lw_pw_filter_connect_inactive_rt(filter: *mut NativeFilter) -> c_int;
    fn lw_pw_filter_node_id(filter: *const NativeFilter) -> u32;
    fn lw_pw_filter_set_active(filter: *mut NativeFilter, active: bool) -> c_int;
    fn lw_pw_filter_disconnect(filter: *mut NativeFilter) -> c_int;
    fn lw_pw_filter_destroy(filter: *mut NativeFilter);
    #[cfg(test)]
    fn lw_pw_filter_semantic_paused() -> c_int;
    #[cfg(test)]
    fn lw_pw_filter_semantic_streaming() -> c_int;
    #[cfg(test)]
    fn lw_pw_filter_semantic_error() -> c_int;
    #[cfg(test)]
    fn lw_pw_test_peer_new(
        core: *mut c_void,
        name: *const c_char,
        properties: *mut c_void,
        port_count: u32,
        process: NativeTestProcessCallback,
        data: *mut c_void,
    ) -> *mut NativeTestPeer;
    #[cfg(test)]
    fn lw_pw_test_peer_add_port(
        peer: *mut NativeTestPeer,
        index: u32,
        output: bool,
        properties: *mut c_void,
    ) -> c_int;
    #[cfg(test)]
    fn lw_pw_test_peer_connect_inactive_rt(peer: *mut NativeTestPeer) -> c_int;
    #[cfg(test)]
    fn lw_pw_test_peer_node_id(peer: *const NativeTestPeer) -> u32;
    #[cfg(test)]
    fn lw_pw_test_peer_set_active(peer: *mut NativeTestPeer, active: bool) -> c_int;
    #[cfg(test)]
    fn lw_pw_test_peer_destroy(peer: *mut NativeTestPeer);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum ProcessFault {
    MissingBuffer = 1,
    InvalidQuantum = 2,
    RateChanged = 3,
    DriverChanged = 4,
    PositionDiscontinuity = 5,
    AliasedBuffer = 6,
    MisalignedBuffer = 7,
    Processor = 8,
    Panic = 9,
    Pause = 10,
    StaleSystemTail = 11,
    SystemPriming = 12,
    SystemDrain = 13,
    SystemLatency = 14,
    BufferExtentOverflow = 15,
    FilterError = 16,
    UnexpectedFilterState = 17,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FaultRecord {
    pub(super) fault: ProcessFault,
    pub(super) driver_id: u32,
    pub(super) position: u64,
    pub(super) expected_position: u64,
    pub(super) quantum: u32,
    pub(super) missing_mask: u32,
}

struct FaultSlot {
    fault: AtomicU8,
    driver_id: AtomicU32,
    position: AtomicU64,
    expected_position: AtomicU64,
    quantum: AtomicU32,
    missing_mask: AtomicU32,
}

impl FaultSlot {
    fn new() -> Self {
        Self {
            fault: AtomicU8::new(0),
            driver_id: AtomicU32::new(0),
            position: AtomicU64::new(0),
            expected_position: AtomicU64::new(0),
            quantum: AtomicU32::new(0),
            missing_mask: AtomicU32::new(0),
        }
    }

    fn store(&self, record: FaultRecord) {
        self.driver_id.store(record.driver_id, Ordering::Relaxed);
        self.position.store(record.position, Ordering::Relaxed);
        self.expected_position.store(record.expected_position, Ordering::Relaxed);
        self.quantum.store(record.quantum, Ordering::Relaxed);
        self.missing_mask.store(record.missing_mask, Ordering::Relaxed);
        self.fault.store(record.fault as u8, Ordering::Release);
    }

    fn load(&self) -> Option<FaultRecord> {
        let fault = match self.fault.load(Ordering::Acquire) {
            1 => ProcessFault::MissingBuffer,
            2 => ProcessFault::InvalidQuantum,
            3 => ProcessFault::RateChanged,
            4 => ProcessFault::DriverChanged,
            5 => ProcessFault::PositionDiscontinuity,
            6 => ProcessFault::AliasedBuffer,
            7 => ProcessFault::MisalignedBuffer,
            8 => ProcessFault::Processor,
            9 => ProcessFault::Panic,
            10 => ProcessFault::Pause,
            11 => ProcessFault::StaleSystemTail,
            12 => ProcessFault::SystemPriming,
            13 => ProcessFault::SystemDrain,
            14 => ProcessFault::SystemLatency,
            15 => ProcessFault::BufferExtentOverflow,
            16 => ProcessFault::FilterError,
            17 => ProcessFault::UnexpectedFilterState,
            _ => return None,
        };
        Some(FaultRecord {
            fault,
            driver_id: self.driver_id.load(Ordering::Relaxed),
            position: self.position.load(Ordering::Relaxed),
            expected_position: self.expected_position.load(Ordering::Relaxed),
            quantum: self.quantum.load(Ordering::Relaxed),
            missing_mask: self.missing_mask.load(Ordering::Relaxed),
        })
    }
}

struct FaultRecords {
    next: AtomicUsize,
    dropped: AtomicU64,
    slots: [FaultSlot; FAULT_CAPACITY],
}

impl FaultRecords {
    fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            slots: std::array::from_fn(|_| FaultSlot::new()),
        }
    }

    fn push(&self, record: FaultRecord) {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = self.slots.get(index) {
            slot.store(record);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> ([Option<FaultRecord>; FAULT_CAPACITY], u64) {
        (
            std::array::from_fn(|index| self.slots[index].load()),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SystemCycleState {
    Idle,
    Priming,
    Active,
    Draining,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ProcessTiming {
    pub(super) driver_id: u32,
    pub(super) rate: u32,
    pub(super) position: u64,
    pub(super) quantum: u32,
}

#[derive(Clone, Copy)]
pub(super) struct StereoInput<'a> {
    pub(super) left: &'a [f32],
    pub(super) right: &'a [f32],
}

pub(super) struct StereoOutput<'a> {
    pub(super) left: &'a mut [f32],
    pub(super) right: &'a mut [f32],
}

pub(super) struct ProcessBlock<'a> {
    pub(super) timing: ProcessTiming,
    pub(super) system_state: SystemCycleState,
    pub(super) system: StereoInput<'a>,
    pub(super) microphone: StereoOutput<'a>,
    pub(super) monitor: StereoOutput<'a>,
    pub(super) stream: StereoOutput<'a>,
}

pub(super) trait GraphProcessor: Send + 'static {
    fn process(&mut self, block: ProcessBlock<'_>) -> Result<(), ProcessFault>;
}

#[derive(Clone, Copy)]
struct CycleValidator {
    expected_driver: u32,
    expected_quantum: NonZeroU32,
    previous_position: Option<u64>,
}

impl CycleValidator {
    fn validate(&mut self, timing: ProcessTiming) -> Result<(), (ProcessFault, u64)> {
        if timing.driver_id != self.expected_driver {
            return Err((ProcessFault::DriverChanged, self.expected_driver.into()));
        }
        if timing.rate != 48_000 {
            return Err((ProcessFault::RateChanged, 48_000));
        }
        if timing.quantum != self.expected_quantum.get() {
            return Err((ProcessFault::InvalidQuantum, self.expected_quantum.get().into()));
        }
        if let Some(expected) = self.previous_position
            && timing.position != expected
        {
            return Err((ProcessFault::PositionDiscontinuity, expected));
        }
        self.previous_position = timing.position.checked_add(u64::from(timing.quantum));
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct SystemTracker {
    observed_word: u64,
    state: SystemCycleState,
}

impl SystemTracker {
    fn advance(&mut self, link_word: u64, system: StereoInput<'_>) -> Result<SystemCycleState, ()> {
        if link_word != self.observed_word {
            let was_present = self.observed_word & 1 == 1;
            let is_present = link_word & 1 == 1;
            self.observed_word = link_word;
            self.state = match (was_present, is_present) {
                (false, true) => SystemCycleState::Priming,
                (true, false) => SystemCycleState::Draining,
                (_, true) => SystemCycleState::Active,
                (_, false) => SystemCycleState::Idle,
            };
        }
        let current = self.state;
        self.state = match current {
            SystemCycleState::Priming => SystemCycleState::Active,
            SystemCycleState::Draining => SystemCycleState::Idle,
            other => other,
        };
        if current == SystemCycleState::Idle
            && system.left.iter().chain(system.right.iter()).any(|sample| *sample != 0.0)
        {
            return Err(());
        }
        Ok(current)
    }
}

struct RealtimeState<P> {
    processor: P,
    validator: CycleValidator,
    system: SystemTracker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BufferRangeError {
    Null,
    Misaligned,
    ExtentOverflow,
    Overlap,
}

fn validate_buffer_ranges(pointers: &[*mut f32], frames: usize) -> Result<(), BufferRangeError> {
    if pointers.len() > PORT_COUNT {
        return Err(BufferRangeError::ExtentOverflow);
    }
    let extent = frames
        .checked_mul(size_of::<f32>())
        .filter(|extent| isize::try_from(*extent).is_ok())
        .ok_or(BufferRangeError::ExtentOverflow)?;
    let mut starts = [0_usize; PORT_COUNT];
    let mut ends = [0_usize; PORT_COUNT];
    for (index, pointer) in pointers.iter().copied().enumerate() {
        if pointer.is_null() {
            return Err(BufferRangeError::Null);
        }
        let start = pointer as usize;
        if !start.is_multiple_of(align_of::<f32>()) {
            return Err(BufferRangeError::Misaligned);
        }
        starts[index] = start;
        ends[index] = start.checked_add(extent).ok_or(BufferRangeError::ExtentOverflow)?;
    }
    for left in 0..pointers.len() {
        for right in (left + 1)..pointers.len() {
            if starts[left] < ends[right] && starts[right] < ends[left] {
                return Err(BufferRangeError::Overlap);
            }
        }
    }
    Ok(())
}

struct CallbackState<P> {
    realtime: UnsafeCell<RealtimeState<P>>,
    faults: FaultRecords,
    link_word: Arc<AtomicU64>,
    failed: AtomicBool,
    callbacks_in_flight: AtomicU32,
    process_cycles: AtomicU64,
    filter_state: AtomicI32,
    has_streamed: AtomicBool,
    stopping: AtomicBool,
}

impl<P> CallbackState<P> {
    fn record(&self, record: FaultRecord) {
        self.faults.push(record);
        self.failed.store(true, Ordering::Release);
    }
}

pub(super) struct FilterDiagnostics {
    pub(super) process_cycles: u64,
    pub(super) filter_state: i32,
    pub(super) failed: bool,
    pub(super) faults: [Option<FaultRecord>; FAULT_CAPACITY],
    pub(super) dropped_faults: u64,
}

pub(super) struct FilterHandle<P: GraphProcessor> {
    native: Option<NonNull<NativeFilter>>,
    callbacks: Pin<Box<CallbackState<P>>>,
    disconnected: bool,
    added_mask: u8,
}

impl<P: GraphProcessor> FilterHandle<P> {
    pub(super) fn new(
        core: &pw::core::Core,
        name: &str,
        properties: pw::properties::PropertiesBox,
        expected_driver: u32,
        expected_quantum: NonZeroU32,
        processor: P,
        link_word: Arc<AtomicU64>,
    ) -> Result<Self, String> {
        let callbacks = Box::pin(CallbackState {
            realtime: UnsafeCell::new(RealtimeState {
                processor,
                validator: CycleValidator {
                    expected_driver,
                    expected_quantum,
                    previous_position: None,
                },
                system: SystemTracker { observed_word: 0, state: SystemCycleState::Idle },
            }),
            faults: FaultRecords::new(),
            link_word,
            failed: AtomicBool::new(false),
            callbacks_in_flight: AtomicU32::new(0),
            process_cycles: AtomicU64::new(0),
            filter_state: AtomicI32::new(0),
            has_streamed: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
        });
        let name = CString::new(name).map_err(|_| "filter name contains a NUL byte".to_owned())?;
        let data = std::ptr::from_ref(&*callbacks).cast_mut().cast::<c_void>();
        // SAFETY: core and callbacks outlive the native filter. The shim consumes
        // properties on every return path and retains data only until destroy.
        let native = unsafe {
            lw_pw_filter_new(
                core.as_raw_ptr().cast(),
                name.as_ptr(),
                properties.into_raw().cast(),
                process_callback::<P>,
                state_callback::<P>,
                data,
            )
        };
        let native = NonNull::new(native).ok_or_else(last_os_error)?;
        Ok(Self { native: Some(native), callbacks, disconnected: false, added_mask: 0 })
    }

    pub(super) fn add_port(
        &mut self,
        index: usize,
        output: bool,
        properties: pw::properties::PropertiesBox,
    ) -> Result<(), String> {
        let native = self.native.ok_or_else(|| "filter is destroyed".to_owned())?;
        if index >= PORT_COUNT {
            return Err("filter port index is out of range".to_owned());
        }
        let bit = 1_u8 << index;
        if self.added_mask & bit != 0 {
            return Err("filter port index is already present".to_owned());
        }
        let index = u32::try_from(index).map_err(|_| "filter port index overflow".to_owned())?;
        // SAFETY: native is owned by self. The shim consumes properties.
        let result = unsafe {
            lw_pw_filter_add_port(native.as_ptr(), index, output, properties.into_raw().cast())
        };
        result_code(result, "add filter port")?;
        self.added_mask |= bit;
        Ok(())
    }

    pub(super) fn connect_inactive(&mut self) -> Result<(), String> {
        let native = self.native.ok_or_else(|| "filter is destroyed".to_owned())?;
        if self.added_mask != u8::MAX {
            return Err("filter does not own all eight ports".to_owned());
        }
        // SAFETY: native is owned by self and all eight ports were added.
        result_code(unsafe { lw_pw_filter_connect_inactive_rt(native.as_ptr()) }, "connect filter")
    }

    pub(super) fn node_id(&self) -> Option<u32> {
        self.native.map(|native| {
            // SAFETY: native remains valid while present in self.
            unsafe { lw_pw_filter_node_id(native.as_ptr()) }
        })
    }

    pub(super) fn set_active(&mut self, active: bool) -> Result<(), String> {
        let native = self.native.ok_or_else(|| "filter is destroyed".to_owned())?;
        if !active {
            self.callbacks.stopping.store(true, Ordering::Release);
        }
        // SAFETY: native is owned by self. Calls occur on the graph control thread.
        result_code(
            unsafe { lw_pw_filter_set_active(native.as_ptr(), active) },
            if active { "activate filter" } else { "deactivate filter" },
        )
    }

    pub(super) fn diagnostics(&self) -> FilterDiagnostics {
        let (faults, dropped_faults) = self.callbacks.faults.snapshot();
        FilterDiagnostics {
            process_cycles: self.callbacks.process_cycles.load(Ordering::Relaxed),
            filter_state: self.callbacks.filter_state.load(Ordering::Acquire),
            failed: self.callbacks.failed.load(Ordering::Acquire),
            faults,
            dropped_faults,
        }
    }

    pub(super) fn callbacks_in_flight(&self) -> u32 {
        self.callbacks.callbacks_in_flight.load(Ordering::Acquire)
    }

    pub(super) fn disconnect(&mut self) -> Result<(), String> {
        if self.disconnected {
            return Ok(());
        }
        let native = self.native.ok_or_else(|| "filter is destroyed".to_owned())?;
        // SAFETY: native is owned by self. PipeWire disconnects the data-loop
        // node before this control-thread call returns.
        result_code(unsafe { lw_pw_filter_disconnect(native.as_ptr()) }, "disconnect filter")?;
        self.disconnected = true;
        Ok(())
    }

    pub(super) fn destroy(&mut self) -> Result<(), String> {
        let disconnect = self.disconnect();
        let Some(native) = self.native.take() else {
            return disconnect;
        };
        // SAFETY: native is consumed exactly once and no callback may use data
        // after the listener is removed and filter destruction completes.
        unsafe { lw_pw_filter_destroy(native.as_ptr()) };
        disconnect
    }
}

impl<P: GraphProcessor> Drop for FilterHandle<P> {
    fn drop(&mut self) {
        if let Some(native) = self.native.take() {
            // SAFETY: callbacks still point at the pinned state retained in
            // self. Native destruction quiesces them before self is released.
            let _ = unsafe { lw_pw_filter_disconnect(native.as_ptr()) };
            unsafe { lw_pw_filter_destroy(native.as_ptr()) };
        }
    }
}

unsafe extern "C" fn state_callback<P: GraphProcessor>(
    data: *mut c_void,
    old: c_int,
    state: c_int,
) {
    // SAFETY: the native bridge retains the pinned CallbackState pointer only
    // between FilterHandle construction and native destruction.
    let callbacks = unsafe { &*data.cast::<CallbackState<P>>() };
    callbacks.filter_state.store(state, Ordering::Release);
    if state == FILTER_STATE_STREAMING {
        callbacks.has_streamed.store(true, Ordering::Release);
        return;
    }
    let has_streamed = callbacks.has_streamed.load(Ordering::Acquire);
    let stopping = callbacks.stopping.load(Ordering::Acquire);
    if let Some(fault) = filter_state_fault(old, state, has_streamed, stopping) {
        callbacks.record(FaultRecord {
            fault,
            driver_id: 0,
            position: 0,
            expected_position: 0,
            quantum: 0,
            missing_mask: 0,
        });
    }
}

fn filter_state_fault(
    old: i32,
    state: i32,
    has_streamed: bool,
    stopping: bool,
) -> Option<ProcessFault> {
    if stopping {
        return None;
    }
    if state == FILTER_STATE_ERROR {
        return Some(ProcessFault::FilterError);
    }
    if has_streamed && old == FILTER_STATE_STREAMING && state != FILTER_STATE_STREAMING {
        return Some(if state == FILTER_STATE_PAUSED {
            ProcessFault::Pause
        } else {
            ProcessFault::UnexpectedFilterState
        });
    }
    None
}

unsafe extern "C" fn process_callback<P: GraphProcessor>(
    data: *mut c_void,
    driver_id: u32,
    rate_num: u32,
    rate_denom: u32,
    position: u64,
    quantum: u32,
    buffers: *mut *mut f32,
    missing_mask: u32,
) -> c_int {
    // SAFETY: the bridge provides its pinned CallbackState for the complete
    // native filter lifetime.
    let callbacks = unsafe { &*data.cast::<CallbackState<P>>() };
    callbacks.callbacks_in_flight.fetch_add(1, Ordering::AcqRel);
    let faulted = if callbacks.failed.load(Ordering::Acquire) {
        true
    } else {
        catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: PipeWire invokes one process callback at a time for this
            // filter. No control-thread code accesses realtime while active.
            let realtime = unsafe { &mut *callbacks.realtime.get() };
            process_cycle(
                callbacks,
                realtime,
                driver_id,
                rate_num,
                rate_denom,
                position,
                quantum,
                buffers,
                missing_mask,
            )
        }))
        .unwrap_or_else(|_| {
            callbacks.record(FaultRecord {
                fault: ProcessFault::Panic,
                driver_id,
                position,
                expected_position: 0,
                quantum,
                missing_mask,
            });
            true
        })
    };
    callbacks.callbacks_in_flight.fetch_sub(1, Ordering::Release);
    c_int::from(faulted)
}

#[allow(clippy::too_many_arguments)]
fn process_cycle<P: GraphProcessor>(
    callbacks: &CallbackState<P>,
    realtime: &mut RealtimeState<P>,
    driver_id: u32,
    rate_num: u32,
    rate_denom: u32,
    position: u64,
    quantum: u32,
    buffers: *mut *mut f32,
    missing_mask: u32,
) -> bool {
    let timing = ProcessTiming {
        driver_id,
        rate: if rate_num == 1 { rate_denom } else { 0 },
        position,
        quantum,
    };
    let fail = |fault, expected_position| {
        callbacks.record(FaultRecord {
            fault,
            driver_id,
            position,
            expected_position,
            quantum,
            missing_mask,
        });
        true
    };
    if missing_mask != 0 || buffers.is_null() {
        return fail(ProcessFault::MissingBuffer, 0);
    }
    if quantum == 0 {
        return fail(ProcessFault::InvalidQuantum, 0);
    }
    if let Err((fault, expected)) = realtime.validator.validate(timing) {
        return fail(fault, expected);
    }
    // SAFETY: the C shim always supplies exactly eight entries.
    let pointers = unsafe { slice::from_raw_parts(buffers, PORT_COUNT) };
    if let Err(error) = validate_buffer_ranges(pointers, quantum as usize) {
        let fault = match error {
            BufferRangeError::Null => ProcessFault::MissingBuffer,
            BufferRangeError::Misaligned => ProcessFault::MisalignedBuffer,
            BufferRangeError::ExtentOverflow => ProcessFault::BufferExtentOverflow,
            BufferRangeError::Overlap => ProcessFault::AliasedBuffer,
        };
        return fail(fault, 0);
    }
    let frames = quantum as usize;
    // SAFETY: pointer validity, alignment, checked byte extents, and pairwise
    // range non-overlap were checked above. PipeWire guarantees `quantum`
    // mapped F32 samples.
    let system_left = unsafe { slice::from_raw_parts(pointers[0], frames) };
    // SAFETY: same checked-range invariant as system_left.
    let system_right = unsafe { slice::from_raw_parts(pointers[1], frames) };
    let system = StereoInput { left: system_left, right: system_right };
    let link_word = callbacks.link_word.load(Ordering::Acquire);
    let Ok(system_state) = realtime.system.advance(link_word, system) else {
        return fail(ProcessFault::StaleSystemTail, 0);
    };
    // SAFETY: all six output ranges are non-null, aligned, fully mapped, and
    // non-overlapping with every input and output range.
    let microphone_left = unsafe { slice::from_raw_parts_mut(pointers[2], frames) };
    // SAFETY: this range was validated as non-overlapping above.
    let microphone_right = unsafe { slice::from_raw_parts_mut(pointers[3], frames) };
    // SAFETY: this range was validated as non-overlapping above.
    let monitor_left = unsafe { slice::from_raw_parts_mut(pointers[4], frames) };
    // SAFETY: this range was validated as non-overlapping above.
    let monitor_right = unsafe { slice::from_raw_parts_mut(pointers[5], frames) };
    // SAFETY: this range was validated as non-overlapping above.
    let stream_left = unsafe { slice::from_raw_parts_mut(pointers[6], frames) };
    // SAFETY: this range was validated as non-overlapping above.
    let stream_right = unsafe { slice::from_raw_parts_mut(pointers[7], frames) };
    let block = ProcessBlock {
        timing,
        system_state,
        system,
        microphone: StereoOutput { left: microphone_left, right: microphone_right },
        monitor: StereoOutput { left: monitor_left, right: monitor_right },
        stream: StereoOutput { left: stream_left, right: stream_right },
    };
    if let Err(fault) = realtime.processor.process(block) {
        return fail(fault, 0);
    }
    callbacks.process_cycles.fetch_add(1, Ordering::Relaxed);
    false
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum TestPeerMode {
    Producer,
    Consumer,
}

#[cfg(test)]
struct TestPeerState {
    mode: TestPeerMode,
    expected_driver: u32,
    expected_quantum: u32,
    cycles: AtomicU64,
    missing: AtomicU64,
    timing_errors: AtomicU64,
    vector_errors: AtomicU64,
    route_errors: AtomicU64,
    panic: AtomicBool,
    output_latency_min: AtomicU32,
    output_latency_max: AtomicU32,
    system_latency_min: AtomicU32,
    system_latency_max: AtomicU32,
    silent_system_cycles: AtomicU64,
    active_system_cycles: AtomicU64,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) struct TestPeerDiagnostics {
    pub(super) cycles: u64,
    pub(super) missing: u64,
    pub(super) timing_errors: u64,
    pub(super) vector_errors: u64,
    pub(super) route_errors: u64,
    pub(super) panic: bool,
    pub(super) output_latency_min: u32,
    pub(super) output_latency_max: u32,
    pub(super) system_latency_min: u32,
    pub(super) system_latency_max: u32,
    pub(super) silent_system_cycles: u64,
    pub(super) active_system_cycles: u64,
}

#[cfg(test)]
pub(super) struct TestPeer {
    native: Option<NonNull<NativeTestPeer>>,
    callbacks: Pin<Box<TestPeerState>>,
    port_count: usize,
}

#[cfg(test)]
impl TestPeer {
    pub(super) fn new(
        core: &pw::core::Core,
        name: &str,
        properties: pw::properties::PropertiesBox,
        mode: TestPeerMode,
        expected_driver: u32,
        expected_quantum: NonZeroU32,
    ) -> Result<Self, String> {
        let port_count = match mode {
            TestPeerMode::Producer => 2,
            TestPeerMode::Consumer => 6,
        };
        let callbacks = Box::pin(TestPeerState {
            mode,
            expected_driver,
            expected_quantum: expected_quantum.get(),
            cycles: AtomicU64::new(0),
            missing: AtomicU64::new(0),
            timing_errors: AtomicU64::new(0),
            vector_errors: AtomicU64::new(0),
            route_errors: AtomicU64::new(0),
            panic: AtomicBool::new(false),
            output_latency_min: AtomicU32::new(u32::MAX),
            output_latency_max: AtomicU32::new(0),
            system_latency_min: AtomicU32::new(u32::MAX),
            system_latency_max: AtomicU32::new(0),
            silent_system_cycles: AtomicU64::new(0),
            active_system_cycles: AtomicU64::new(0),
        });
        let name =
            CString::new(name).map_err(|_| "test peer name contains a NUL byte".to_owned())?;
        let data = std::ptr::from_ref(&*callbacks).cast_mut().cast::<c_void>();
        // SAFETY: callbacks is pinned and outlives the peer. The C bridge
        // consumes properties and retains data only until destroy.
        let native = unsafe {
            lw_pw_test_peer_new(
                core.as_raw_ptr().cast(),
                name.as_ptr(),
                properties.into_raw().cast(),
                u32::try_from(port_count).expect("test peer port count fits u32"),
                test_peer_callback,
                data,
            )
        };
        let native = NonNull::new(native).ok_or_else(last_os_error)?;
        Ok(Self { native: Some(native), callbacks, port_count })
    }

    pub(super) fn add_port(
        &mut self,
        index: usize,
        output: bool,
        name: &str,
        channel: &str,
    ) -> Result<(), String> {
        if index >= self.port_count {
            return Err("test peer port index is out of range".to_owned());
        }
        let native = self.native.ok_or_else(|| "test peer is destroyed".to_owned())?;
        let properties = pw::properties::properties! {
            *pw::keys::FORMAT_DSP => "32 bit float mono audio",
            *pw::keys::PORT_NAME => name,
            *pw::keys::AUDIO_CHANNEL => channel
        };
        // SAFETY: native is owned by self. The C bridge consumes properties.
        result_code(
            unsafe {
                lw_pw_test_peer_add_port(
                    native.as_ptr(),
                    u32::try_from(index).expect("test peer index fits u32"),
                    output,
                    properties.into_raw().cast(),
                )
            },
            "add test peer port",
        )
    }

    pub(super) fn connect_inactive(&mut self) -> Result<(), String> {
        let native = self.native.ok_or_else(|| "test peer is destroyed".to_owned())?;
        // SAFETY: native is owned by self and all planned ports were added.
        result_code(
            unsafe { lw_pw_test_peer_connect_inactive_rt(native.as_ptr()) },
            "connect test peer",
        )
    }

    pub(super) fn node_id(&self) -> u32 {
        let native = self.native.expect("test peer is live");
        // SAFETY: native remains valid while present in self.
        unsafe { lw_pw_test_peer_node_id(native.as_ptr()) }
    }

    pub(super) fn set_active(&mut self, active: bool) -> Result<(), String> {
        let native = self.native.ok_or_else(|| "test peer is destroyed".to_owned())?;
        // SAFETY: native is owned by self and the call is on the control thread.
        result_code(
            unsafe { lw_pw_test_peer_set_active(native.as_ptr(), active) },
            if active { "activate test peer" } else { "deactivate test peer" },
        )
    }

    pub(super) fn diagnostics(&self) -> TestPeerDiagnostics {
        let minimum = |value: &AtomicU32| match value.load(Ordering::Relaxed) {
            u32::MAX => 0,
            value => value,
        };
        TestPeerDiagnostics {
            cycles: self.callbacks.cycles.load(Ordering::Relaxed),
            missing: self.callbacks.missing.load(Ordering::Relaxed),
            timing_errors: self.callbacks.timing_errors.load(Ordering::Relaxed),
            vector_errors: self.callbacks.vector_errors.load(Ordering::Relaxed),
            route_errors: self.callbacks.route_errors.load(Ordering::Relaxed),
            panic: self.callbacks.panic.load(Ordering::Relaxed),
            output_latency_min: minimum(&self.callbacks.output_latency_min),
            output_latency_max: self.callbacks.output_latency_max.load(Ordering::Relaxed),
            system_latency_min: minimum(&self.callbacks.system_latency_min),
            system_latency_max: self.callbacks.system_latency_max.load(Ordering::Relaxed),
            silent_system_cycles: self.callbacks.silent_system_cycles.load(Ordering::Relaxed),
            active_system_cycles: self.callbacks.active_system_cycles.load(Ordering::Relaxed),
        }
    }

    pub(super) fn destroy(&mut self) {
        if let Some(native) = self.native.take() {
            // SAFETY: peer callbacks are quiescent after deactivation and graph
            // synchronization in the integration owner.
            unsafe { lw_pw_test_peer_destroy(native.as_ptr()) };
        }
    }
}

#[cfg(test)]
impl Drop for TestPeer {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
unsafe extern "C" fn test_peer_callback(
    data: *mut c_void,
    driver_id: u32,
    rate_num: u32,
    rate_denom: u32,
    position: u64,
    quantum: u32,
    buffers: *mut *mut f32,
    missing_mask: u32,
) {
    // SAFETY: the test bridge retains this pinned state only until destroy.
    let state = unsafe { &*data.cast::<TestPeerState>() };
    if catch_unwind(AssertUnwindSafe(|| {
        test_peer_cycle(
            state,
            driver_id,
            rate_num,
            rate_denom,
            position,
            quantum,
            buffers,
            missing_mask,
        );
    }))
    .is_err()
    {
        state.panic.store(true, Ordering::Release);
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::cast_precision_loss)]
fn test_peer_cycle(
    state: &TestPeerState,
    driver_id: u32,
    rate_num: u32,
    rate_denom: u32,
    position: u64,
    quantum: u32,
    buffers: *mut *mut f32,
    missing_mask: u32,
) {
    let port_count = match state.mode {
        TestPeerMode::Producer => 2,
        TestPeerMode::Consumer => 6,
    };
    if missing_mask != 0 || buffers.is_null() {
        state.missing.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if driver_id != state.expected_driver
        || rate_num != 1
        || rate_denom != 48_000
        || quantum != state.expected_quantum
        || quantum == 0
    {
        state.timing_errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: the C bridge supplies the mode's fixed number of mapped ports.
    let pointers = unsafe { slice::from_raw_parts(buffers, port_count) };
    if let Err(error) = validate_buffer_ranges(pointers, quantum as usize) {
        if error == BufferRangeError::Null {
            state.missing.fetch_add(1, Ordering::Relaxed);
        } else {
            state.vector_errors.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    let frames = quantum as usize;
    match state.mode {
        TestPeerMode::Producer => {
            // SAFETY: both checked ranges are non-overlapping and contain
            // `frames` writable F32 samples for this callback.
            let left = unsafe { slice::from_raw_parts_mut(pointers[0], frames) };
            // SAFETY: the right range passed the same validation.
            let right = unsafe { slice::from_raw_parts_mut(pointers[1], frames) };
            for (index, (left, right)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
                let frame = position + index as u64;
                *left = 0.25 + (frame % 983) as f32 / 16_384.0;
                *right = 0.375 + (frame % 977) as f32 / 16_384.0;
            }
        }
        TestPeerMode::Consumer => consume_test_vectors(state, pointers, frames, position),
    }
    state.cycles.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::float_cmp, clippy::needless_range_loop)]
fn consume_test_vectors(
    state: &TestPeerState,
    pointers: &[*mut f32],
    frames: usize,
    position: u64,
) {
    // SAFETY: all six ranges were checked before this function was called and
    // are mapped input buffers with `frames` samples.
    let buffers: [&[f32]; 6] =
        std::array::from_fn(|index| unsafe { slice::from_raw_parts(pointers[index], frames) });
    let mut active_system = false;
    for index in 0..frames {
        let mic_left_code = ((buffers[0][index] - 0.125) * 8192.0).round() as i32;
        let mic_right_code = ((buffers[1][index] - 0.1875) * 8192.0).round() as i32;
        let Some(dsp_position) = decode_position(mic_left_code, mic_right_code, 997, 991, 826)
        else {
            state.vector_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let consumer_position = ((position + index as u64) % 988_027) as u32;
        let output_latency = (consumer_position + 988_027 - dsp_position) % 988_027;
        state.output_latency_min.fetch_min(output_latency, Ordering::Relaxed);
        state.output_latency_max.fetch_max(output_latency, Ordering::Relaxed);
        if buffers[2][index] != buffers[4][index] || buffers[3][index] != buffers[5][index] {
            state.route_errors.fetch_add(1, Ordering::Relaxed);
        }
        let system_left = buffers[2][index] - buffers[0][index];
        let system_right = buffers[3][index] - buffers[1][index];
        if system_left != 0.0 || system_right != 0.0 {
            active_system = true;
            let left_code = ((system_left - 0.25) * 16_384.0).round() as i32;
            let right_code = ((system_right - 0.375) * 16_384.0).round() as i32;
            if let Some(producer_position) = decode_position(left_code, right_code, 983, 977, 163) {
                let system_latency = (consumer_position + 960_391 - producer_position) % 960_391;
                state.system_latency_min.fetch_min(system_latency, Ordering::Relaxed);
                state.system_latency_max.fetch_max(system_latency, Ordering::Relaxed);
            } else {
                state.vector_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if active_system {
        state.active_system_cycles.fetch_add(1, Ordering::Relaxed);
    } else {
        state.silent_system_cycles.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
fn decode_position(
    left: i32,
    right: i32,
    left_mod: u32,
    right_mod: u32,
    inverse: u32,
) -> Option<u32> {
    let left_limit = i32::try_from(left_mod).ok()?;
    let right_limit = i32::try_from(right_mod).ok()?;
    if left < 0 || right < 0 || left >= left_limit || right >= right_limit {
        return None;
    }
    let difference = u32::try_from((right - left + right_limit) % right_limit).ok()?;
    Some(u32::try_from(left).ok()? + left_mod * ((difference * inverse) % right_mod))
}

fn result_code(result: c_int, operation: &str) -> Result<(), String> {
    if result < 0 {
        Err(format!("{operation} failed with PipeWire result {result}"))
    } else {
        Ok(())
    }
}

fn last_os_error() -> String {
    std::io::Error::last_os_error().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_shim_owns_pipewire_state_mapping() {
        // SAFETY: these pure functions return the C shim's semantic constants.
        let paused = unsafe { lw_pw_filter_semantic_paused() };
        // SAFETY: same invariant as the paused mapping query.
        let streaming = unsafe { lw_pw_filter_semantic_streaming() };
        // SAFETY: same invariant as the paused mapping query.
        let error = unsafe { lw_pw_filter_semantic_error() };

        assert_eq!(paused, FILTER_STATE_PAUSED);
        assert_eq!(streaming, FILTER_STATE_STREAMING);
        assert_eq!(error, FILTER_STATE_ERROR);
        assert_ne!(FILTER_STATE_OTHER, FILTER_STATE_ERROR);
    }

    #[test]
    fn filter_state_errors_and_unexpected_departures_are_attempt_faults() {
        assert_eq!(filter_state_fault(FILTER_STATE_OTHER, FILTER_STATE_PAUSED, false, false), None);
        assert_eq!(
            filter_state_fault(FILTER_STATE_OTHER, FILTER_STATE_ERROR, false, false),
            Some(ProcessFault::FilterError)
        );
        assert_eq!(
            filter_state_fault(FILTER_STATE_STREAMING, FILTER_STATE_PAUSED, true, false,),
            Some(ProcessFault::Pause)
        );
        assert_eq!(
            filter_state_fault(FILTER_STATE_STREAMING, FILTER_STATE_OTHER, true, false),
            Some(ProcessFault::UnexpectedFilterState)
        );
        assert_eq!(
            filter_state_fault(FILTER_STATE_STREAMING, FILTER_STATE_ERROR, true, true),
            None
        );
    }

    #[test]
    fn buffer_ranges_reject_shifted_overlap() {
        let mut samples = [0.0_f32; 8];
        let base = samples.as_mut_ptr();
        // SAFETY: the pointer remains within `samples` and is not dereferenced.
        let shifted = unsafe { base.add(1) };

        assert_eq!(validate_buffer_ranges(&[base, shifted], 4), Err(BufferRangeError::Overlap));
    }

    #[test]
    fn buffer_ranges_reject_address_extent_overflow() {
        let aligned_max = usize::MAX & !(align_of::<f32>() - 1);
        let pointer = aligned_max as *mut f32;

        assert_eq!(validate_buffer_ranges(&[pointer], 1), Err(BufferRangeError::ExtentOverflow));
    }

    #[test]
    fn buffer_ranges_accept_adjacent_allocations() {
        let mut samples = [0.0_f32; 8];
        let first = samples.as_mut_ptr();
        // SAFETY: the pointer selects the adjacent second half of `samples`
        // and is not dereferenced by the validator.
        let second = unsafe { first.add(4) };

        assert_eq!(validate_buffer_ranges(&[first, second], 4), Ok(()));
    }

    #[test]
    fn timing_rejects_quantum_and_position_changes() {
        let mut validator = CycleValidator {
            expected_driver: 25,
            expected_quantum: NonZeroU32::new(128).expect("nonzero"),
            previous_position: None,
        };
        assert_eq!(
            validator.validate(ProcessTiming {
                driver_id: 25,
                rate: 48_000,
                position: 256,
                quantum: 128,
            }),
            Ok(())
        );
        assert_eq!(
            validator.validate(ProcessTiming {
                driver_id: 25,
                rate: 48_000,
                position: 384,
                quantum: 64,
            }),
            Err((ProcessFault::InvalidQuantum, 128))
        );
        assert_eq!(
            validator.validate(ProcessTiming {
                driver_id: 25,
                rate: 48_000,
                position: 512,
                quantum: 128,
            }),
            Err((ProcessFault::PositionDiscontinuity, 384))
        );
    }

    #[test]
    fn system_link_generation_has_one_priming_and_drain_cycle() {
        let silence = [0.0; 2];
        let active = [0.25; 2];
        let mut tracker = SystemTracker { observed_word: 0, state: SystemCycleState::Idle };
        let input = |samples| StereoInput { left: samples, right: samples };

        assert_eq!(tracker.advance(0, input(&silence)), Ok(SystemCycleState::Idle));
        assert_eq!(tracker.advance(3, input(&silence)), Ok(SystemCycleState::Priming));
        assert_eq!(tracker.advance(3, input(&active)), Ok(SystemCycleState::Active));
        assert_eq!(tracker.advance(4, input(&active)), Ok(SystemCycleState::Draining));
        assert_eq!(tracker.advance(4, input(&silence)), Ok(SystemCycleState::Idle));
        assert_eq!(tracker.advance(4, input(&active)), Err(()));
    }

    #[test]
    fn fault_records_are_fixed_capacity() {
        let records = FaultRecords::new();
        let record = FaultRecord {
            fault: ProcessFault::MissingBuffer,
            driver_id: 25,
            position: 128,
            expected_position: 0,
            quantum: 128,
            missing_mask: 1,
        };
        for _ in 0..FAULT_CAPACITY + 2 {
            records.push(record);
        }
        let (snapshot, dropped) = records.snapshot();
        assert!(snapshot.iter().all(Option::is_some));
        assert_eq!(dropped, 2);
    }
}
