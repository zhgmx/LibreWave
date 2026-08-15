use crate::config::{MAX_SOURCES, MixerConfig, SourceId};
use crate::{SourceControls, controls::MixRoute};
use std::cell::UnsafeCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const FULL: u8 = 2;

/// The single producer for the bounded control handoff.
///
/// The handoff holds one complete pending snapshot. `ControlStager` is not
/// cloneable, and staging requires mutable access. One producer may move the
/// handle between threads, but concurrent producers are outside the contract.
/// A full slot is never overwritten.
#[derive(Debug)]
pub struct ControlStager {
    config: MixerConfig,
    slot: Arc<ControlSlot>,
}

impl ControlStager {
    /// Validates and stages a complete snapshot for the next nonzero block.
    ///
    /// Fader coefficients are calculated here, outside realtime processing.
    ///
    /// # Errors
    ///
    /// Returns a mapping error for an incoherent source-control set or
    /// [`StageError::Full`] while a snapshot is pending.
    pub fn try_stage(&mut self, controls: &[SourceControls]) -> Result<(), StageError> {
        let snapshot = compile_snapshot(&self.config, controls).map_err(StageError::Controls)?;
        self.slot
            .state
            .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| StageError::Full)?;

        // SAFETY: the successful EMPTY-to-WRITING transition gives the single
        // producer exclusive write access. The consumer reads only after the
        // FULL store publishes this complete Copy value with Release ordering.
        unsafe { *self.slot.value.get() = snapshot };
        self.slot.state.store(FULL, Ordering::Release);
        Ok(())
    }
}

/// Why a complete source-control mapping is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlMappingError {
    ControlCount { expected: usize, actual: usize },
    UnknownSource(SourceId),
    DuplicateSource(SourceId),
}

impl fmt::Display for ControlMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlCount { expected, actual } => {
                write!(formatter, "expected {expected} source controls, received {actual}")
            }
            Self::UnknownSource(source) => write!(formatter, "source {source} is not configured"),
            Self::DuplicateSource(source) => {
                write!(formatter, "duplicate source {source} controls")
            }
        }
    }
}

impl std::error::Error for ControlMappingError {}

/// Why a control snapshot could not be staged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageError {
    Controls(ControlMappingError),
    Full,
}

impl fmt::Display for StageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Controls(error) => write!(formatter, "invalid mixer controls: {error}"),
            Self::Full => formatter.write_str("the pending mixer-control slot is full"),
        }
    }
}

impl std::error::Error for StageError {}

#[derive(Debug)]
pub(crate) struct ControlConsumer {
    slot: Arc<ControlSlot>,
}

impl ControlConsumer {
    pub(crate) fn channel(config: MixerConfig) -> (ControlStager, Self) {
        let slot = Arc::new(ControlSlot {
            state: AtomicU8::new(EMPTY),
            value: UnsafeCell::new(ControlSnapshot::EMPTY),
        });
        (ControlStager { config, slot: Arc::clone(&slot) }, Self { slot })
    }

    /// Consumes the published snapshot without cloning or dropping the Arc.
    pub(crate) fn take(&self) -> Option<ControlSnapshot> {
        if self.slot.state.load(Ordering::Acquire) != FULL {
            return None;
        }

        // SAFETY: the Acquire load observes the producer's complete write
        // before its Release store to FULL. The producer cannot write again
        // until this sole consumer returns the slot to EMPTY after the copy.
        let snapshot = unsafe { *self.slot.value.get() };
        self.slot.state.store(EMPTY, Ordering::Release);
        Some(snapshot)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimeRoute {
    pub(crate) coefficient: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ControlSnapshot {
    pub(crate) monitor: [RuntimeRoute; MAX_SOURCES],
    pub(crate) stream: [RuntimeRoute; MAX_SOURCES],
}

impl ControlSnapshot {
    const EMPTY: Self = Self {
        monitor: [RuntimeRoute { coefficient: 0.0 }; MAX_SOURCES],
        stream: [RuntimeRoute { coefficient: 0.0 }; MAX_SOURCES],
    };
}

pub(crate) fn compile_snapshot(
    config: &MixerConfig,
    controls: &[SourceControls],
) -> Result<ControlSnapshot, ControlMappingError> {
    if controls.len() != config.sources().len() {
        return Err(ControlMappingError::ControlCount {
            expected: config.sources().len(),
            actual: controls.len(),
        });
    }

    let mut snapshot = ControlSnapshot::EMPTY;
    let mut present = [false; MAX_SOURCES];
    for controls in controls {
        let Some(index) = config.source_index(controls.source()) else {
            return Err(ControlMappingError::UnknownSource(controls.source()));
        };
        if present[index] {
            return Err(ControlMappingError::DuplicateSource(controls.source()));
        }
        present[index] = true;
        snapshot.monitor[index] = compile_route(controls.monitor());
        snapshot.stream[index] = compile_route(controls.stream());
    }
    Ok(snapshot)
}

fn compile_route(route: MixRoute) -> RuntimeRoute {
    RuntimeRoute {
        coefficient: if route.enabled() { route.fader().linear_coefficient() } else { 0.0 },
    }
}

#[derive(Debug)]
struct ControlSlot {
    state: AtomicU8,
    value: UnsafeCell<ControlSnapshot>,
}

// SAFETY: the SPSC state machine governs every access to value. The producer
// owns WRITING exclusively. The sole consumer reads only FULL and publishes
// EMPTY only after its copy completes.
unsafe impl Sync for ControlSlot {}
