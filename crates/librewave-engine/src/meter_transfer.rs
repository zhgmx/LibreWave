use crate::BlockMeters;
use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const FULL: u8 = 2;

/// The single producer for a bounded meter handoff.
///
/// The slot holds at most one processed-block observation. A full slot is not
/// overwritten, so a slow reader cannot block realtime processing.
#[derive(Debug)]
pub struct MeterPublisher {
    slot: Arc<MeterSlot>,
}

impl MeterPublisher {
    /// Creates one empty, single-producer/single-consumer meter slot.
    #[must_use]
    pub fn channel() -> (Self, MeterReader) {
        let slot =
            Arc::new(MeterSlot { state: AtomicU8::new(EMPTY), value: UnsafeCell::new(None) });
        (Self { slot: Arc::clone(&slot) }, MeterReader { slot })
    }

    /// Publishes one real processed-block observation without waiting.
    ///
    /// Returns false when an older observation is pending.
    #[must_use]
    pub fn try_publish(&mut self, meters: BlockMeters) -> bool {
        if self
            .slot
            .state
            .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }

        // SAFETY: the successful EMPTY-to-WRITING transition gives this sole
        // producer exclusive access. The consumer reads only after FULL is
        // published with Release ordering.
        unsafe { *self.slot.value.get() = Some(meters) };
        self.slot.state.store(FULL, Ordering::Release);
        true
    }
}

/// The single consumer for a bounded meter handoff.
#[derive(Debug)]
pub struct MeterReader {
    slot: Arc<MeterSlot>,
}

impl MeterReader {
    /// Takes the pending processed-block observation, when one exists.
    ///
    /// An empty slot means that no new live observation is available. It does
    /// not represent a zero-valued meter block.
    pub fn try_take(&mut self) -> Option<BlockMeters> {
        if self.slot.state.load(Ordering::Acquire) != FULL {
            return None;
        }

        // SAFETY: the Acquire load observes the producer's complete write.
        // The producer cannot write again until this sole consumer publishes
        // EMPTY after taking the Copy value.
        let meters = unsafe { (*self.slot.value.get()).take() };
        self.slot.state.store(EMPTY, Ordering::Release);
        meters
    }
}

#[derive(Debug)]
struct MeterSlot {
    state: AtomicU8,
    value: UnsafeCell<Option<BlockMeters>>,
}

// SAFETY: the SPSC state machine gives the producer and consumer exclusive
// access to value in WRITING and FULL states respectively.
unsafe impl Sync for MeterSlot {}
