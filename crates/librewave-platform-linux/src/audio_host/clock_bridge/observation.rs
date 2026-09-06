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
pub(in crate::audio_host) enum CopySlotError {
    Full,
    Empty,
}

pub(in crate::audio_host) struct CopySlotPublisher<T: Copy> {
    shared: Arc<CopySlot<T>>,
}

impl<T: Copy> fmt::Debug for CopySlotPublisher<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CopySlotPublisher")
    }
}

impl<T: Copy> CopySlotPublisher<T> {
    pub(in crate::audio_host) fn prepare_publish(
        &mut self,
        value: T,
    ) -> Result<PreparedCopySlotPublish<'_, T>, CopySlotError> {
        if self.shared.state.load(Ordering::Acquire) != EMPTY {
            return Err(CopySlotError::Full);
        }
        Ok(PreparedCopySlotPublish { publisher: self, value })
    }

    pub(in crate::audio_host) fn try_publish(&mut self, value: T) -> Result<(), CopySlotError> {
        self.prepare_publish(value)?.commit();
        Ok(())
    }
}

pub(in crate::audio_host) struct PreparedCopySlotPublish<'a, T: Copy> {
    publisher: &'a mut CopySlotPublisher<T>,
    value: T,
}

impl<T: Copy> PreparedCopySlotPublish<'_, T> {
    pub(in crate::audio_host) fn commit(self) {
        // SAFETY: there is one publisher and EMPTY proves that the reader does
        // not own this slot. Release publication happens after the copy.
        unsafe {
            (*self.publisher.shared.value.get()).write(self.value);
        }
        self.publisher.shared.state.store(FULL, Ordering::Release);
    }
}

pub(in crate::audio_host) struct CopySlotReader<T: Copy> {
    shared: Arc<CopySlot<T>>,
}

impl<T: Copy> CopySlotReader<T> {
    pub(in crate::audio_host) fn peek(
        &mut self,
    ) -> Result<PreparedCopySlotRead<'_, T>, CopySlotError> {
        if self.shared.state.load(Ordering::Acquire) != FULL {
            return Err(CopySlotError::Empty);
        }
        // SAFETY: the acquired FULL state proves that the publisher completed
        // the only write, and it cannot write again until this reader commits.
        let value = unsafe { (*self.shared.value.get()).assume_init_read() };
        Ok(PreparedCopySlotRead { reader: self, value })
    }

    pub(in crate::audio_host) fn try_take(&mut self) -> Result<T, CopySlotError> {
        let read = self.peek()?;
        let value = read.value();
        read.commit();
        Ok(value)
    }
}

pub(in crate::audio_host) struct PreparedCopySlotRead<'a, T: Copy> {
    reader: &'a mut CopySlotReader<T>,
    value: T,
}

impl<T: Copy> PreparedCopySlotRead<'_, T> {
    pub(in crate::audio_host) const fn value(&self) -> T {
        self.value
    }

    pub(in crate::audio_host) fn commit(self) {
        self.reader.shared.state.store(EMPTY, Ordering::Release);
    }
}

pub(in crate::audio_host) fn copy_slot<T: Copy>() -> (CopySlotPublisher<T>, CopySlotReader<T>) {
    let shared = Arc::new(CopySlot {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        state: AtomicU8::new(EMPTY),
    });
    (CopySlotPublisher { shared: Arc::clone(&shared) }, CopySlotReader { shared })
}

struct CopySlot<T: Copy> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicU8,
}

// SAFETY: the slot has exactly one publisher and one reader. Release/acquire
// state publication transfers access to the single `Copy` value.
unsafe impl<T: Copy + Send> Sync for CopySlot<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::drop_non_drop)]
    fn publication_and_consumption_are_transactional_and_do_not_overwrite() {
        let (mut publisher, mut reader) = copy_slot::<u64>();
        drop(publisher.prepare_publish(1).expect("first preview"));
        assert!(matches!(reader.peek(), Err(CopySlotError::Empty)));

        publisher.prepare_publish(2).expect("second preview").commit();
        assert!(matches!(publisher.prepare_publish(3), Err(CopySlotError::Full)));
        let read = reader.peek().expect("published value");
        assert_eq!(read.value(), 2);
        drop(read);
        assert!(matches!(publisher.prepare_publish(3), Err(CopySlotError::Full)));

        reader.peek().expect("same published value").commit();
        publisher.prepare_publish(3).expect("slot released").commit();
        assert_eq!(reader.peek().expect("next value").value(), 3);
    }

    #[test]
    fn direct_publication_drops_new_values_while_full() {
        let (mut publisher, mut reader) = copy_slot::<u64>();
        publisher.try_publish(7).expect("empty slot");
        assert_eq!(publisher.try_publish(8), Err(CopySlotError::Full));
        assert_eq!(reader.try_take(), Ok(7));
        assert_eq!(reader.try_take(), Err(CopySlotError::Empty));
        publisher.try_publish(9).expect("released slot");
        assert_eq!(reader.try_take(), Ok(9));
    }
}
