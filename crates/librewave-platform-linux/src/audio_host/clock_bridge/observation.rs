use librewave_engine::{ClockFramePosition, ClockObservation};
use std::cell::UnsafeCell;
use std::fmt;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const EMPTY: u8 = 0;
const FULL: u8 = 1;

/// One exact Wave capture clock sample supplied by the capture worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureClockObservation {
    clock: ClockObservation,
}

impl CaptureClockObservation {
    #[must_use]
    pub const fn new(clock: ClockObservation) -> Self {
        Self { clock }
    }

    #[must_use]
    pub const fn clock(self) -> ClockObservation {
        self.clock
    }
}

/// One exact `PipeWire` graph position and quantum sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphClockObservation {
    clock: ClockObservation,
    quantum_frames: usize,
}

impl GraphClockObservation {
    /// Constructs an observation. A graph quantum must contain at least one
    /// frame; the configured attempt ceiling is checked by the processor.
    ///
    /// # Errors
    ///
    /// Returns an error when `quantum_frames` is zero.
    pub const fn try_new(
        clock: ClockObservation,
        quantum_frames: usize,
    ) -> Result<Self, GraphClockObservationError> {
        if quantum_frames == 0 {
            return Err(GraphClockObservationError::ZeroQuantum);
        }
        Ok(Self { clock, quantum_frames })
    }

    #[must_use]
    pub const fn clock(self) -> ClockObservation {
        self.clock
    }

    #[must_use]
    pub const fn quantum_frames(self) -> usize {
        self.quantum_frames
    }
}

/// Why a graph observation was rejected before processing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphClockObservationError {
    ZeroQuantum,
}

impl fmt::Display for GraphClockObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("graph clock observation quantum must be nonzero")
    }
}

impl std::error::Error for GraphClockObservationError {}

/// One exact Wave playback hardware sample and its ALSA application position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackClockObservation {
    hardware: ClockObservation,
    application_frame_position: ClockFramePosition,
}

impl PlaybackClockObservation {
    #[must_use]
    pub const fn new(
        hardware: ClockObservation,
        application_frame_position: ClockFramePosition,
    ) -> Self {
        Self { hardware, application_frame_position }
    }

    #[must_use]
    pub const fn hardware(self) -> ClockObservation {
        self.hardware
    }

    #[must_use]
    pub const fn application_frame_position(self) -> ClockFramePosition {
        self.application_frame_position
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ObservationHandoffError {
    Full,
    Empty,
}

pub(super) struct ObservationPublisher<T: Copy> {
    shared: Arc<ObservationSlot<T>>,
}

impl<T: Copy> fmt::Debug for ObservationPublisher<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ObservationPublisher")
    }
}

impl<T: Copy> ObservationPublisher<T> {
    pub(super) fn prepare_publish(
        &mut self,
        value: T,
    ) -> Result<PreparedObservationPublish<'_, T>, ObservationHandoffError> {
        if self.shared.state.load(Ordering::Acquire) != EMPTY {
            return Err(ObservationHandoffError::Full);
        }
        Ok(PreparedObservationPublish { publisher: self, value })
    }
}

pub(super) struct PreparedObservationPublish<'a, T: Copy> {
    publisher: &'a mut ObservationPublisher<T>,
    value: T,
}

impl<T: Copy> PreparedObservationPublish<'_, T> {
    pub(super) fn commit(self) {
        // SAFETY: there is one publisher and EMPTY proves that the reader does
        // not own this slot. Release publication happens after the copy.
        unsafe {
            (*self.publisher.shared.value.get()).write(self.value);
        }
        self.publisher.shared.state.store(FULL, Ordering::Release);
    }
}

pub(super) struct ObservationReader<T: Copy> {
    shared: Arc<ObservationSlot<T>>,
}

impl<T: Copy> ObservationReader<T> {
    pub(super) fn peek(
        &mut self,
    ) -> Result<PreparedObservationRead<'_, T>, ObservationHandoffError> {
        if self.shared.state.load(Ordering::Acquire) != FULL {
            return Err(ObservationHandoffError::Empty);
        }
        // SAFETY: the acquired FULL state proves that the publisher completed
        // the only write, and it cannot write again until this reader commits.
        let value = unsafe { (*self.shared.value.get()).assume_init_read() };
        Ok(PreparedObservationRead { reader: self, value })
    }
}

pub(super) struct PreparedObservationRead<'a, T: Copy> {
    reader: &'a mut ObservationReader<T>,
    value: T,
}

impl<T: Copy> PreparedObservationRead<'_, T> {
    pub(super) const fn value(&self) -> T {
        self.value
    }

    pub(super) fn commit(self) {
        self.reader.shared.state.store(EMPTY, Ordering::Release);
    }
}

pub(super) fn channel<T: Copy>() -> (ObservationPublisher<T>, ObservationReader<T>) {
    let shared = Arc::new(ObservationSlot {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        state: AtomicU8::new(EMPTY),
    });
    (ObservationPublisher { shared: Arc::clone(&shared) }, ObservationReader { shared })
}

struct ObservationSlot<T: Copy> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicU8,
}

// SAFETY: the slot has exactly one publisher and one reader. Release/acquire
// state publication transfers access to the single `Copy` value.
unsafe impl<T: Copy + Send> Sync for ObservationSlot<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::drop_non_drop)]
    fn publication_and_consumption_are_transactional_and_do_not_overwrite() {
        let (mut publisher, mut reader) = channel::<u64>();
        drop(publisher.prepare_publish(1).expect("first preview"));
        assert!(matches!(reader.peek(), Err(ObservationHandoffError::Empty)));

        publisher.prepare_publish(2).expect("second preview").commit();
        assert!(matches!(publisher.prepare_publish(3), Err(ObservationHandoffError::Full)));
        let read = reader.peek().expect("published value");
        assert_eq!(read.value(), 2);
        drop(read);
        assert!(matches!(publisher.prepare_publish(3), Err(ObservationHandoffError::Full)));

        reader.peek().expect("same published value").commit();
        publisher.prepare_publish(3).expect("slot released").commit();
        assert_eq!(reader.peek().expect("next value").value(), 3);
    }
}
