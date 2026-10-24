use crate::config::{MAX_SOURCES, MixerConfig};
use librewave_core::{MixRoute, MixTarget, OutputControls, SourceControls, SourceId};
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
    pub fn try_stage(
        &mut self,
        controls: &[SourceControls],
        outputs: OutputControls,
    ) -> Result<(), StageError> {
        self.try_prepare(controls, outputs)?.commit();
        Ok(())
    }

    /// Reserves a complete update without making it visible to the audio thread.
    ///
    /// Dropping the returned transaction cancels it. Commit only after graph
    /// preparation and profile persistence succeed. The consumer keeps using
    /// its previous controls while the transaction is prepared.
    ///
    /// # Errors
    /// Returns a mapping error for invalid controls or [`StageError::Full`]
    /// when another prepared or published update occupies the slot.
    pub fn try_prepare(
        &mut self,
        controls: &[SourceControls],
        outputs: OutputControls,
    ) -> Result<PreparedControls, StageError> {
        let snapshot =
            compile_snapshot(&self.config, controls, outputs).map_err(StageError::Controls)?;
        self.slot
            .state
            .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| StageError::Full)?;
        // SAFETY: WRITING gives this transaction exclusive access. The
        // consumer cannot read the value until commit publishes FULL.
        unsafe { *self.slot.value.get() = snapshot };
        Ok(PreparedControls { slot: Arc::clone(&self.slot), committed: false })
    }
}

/// A control-side transaction that owns the reserved handoff slot.
///
/// This value must stay off the audio thread: dropping it releases an Arc.
#[derive(Debug)]
#[must_use = "commit the prepared controls or drop them to cancel"]
pub struct PreparedControls {
    slot: Arc<ControlSlot>,
    committed: bool,
}

impl PreparedControls {
    /// Publishes the complete update for the next accepted audio block.
    pub fn commit(mut self) {
        self.slot.state.store(FULL, Ordering::Release);
        self.committed = true;
    }
}

impl Drop for PreparedControls {
    fn drop(&mut self) {
        if !self.committed {
            self.slot.state.store(EMPTY, Ordering::Release);
        }
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
    pub(crate) outputs: [RuntimeRoute; 2],
}

impl ControlSnapshot {
    const EMPTY: Self = Self {
        monitor: [RuntimeRoute { coefficient: 0.0 }; MAX_SOURCES],
        stream: [RuntimeRoute { coefficient: 0.0 }; MAX_SOURCES],
        outputs: [RuntimeRoute { coefficient: 0.0 }; 2],
    };
}

pub(crate) fn compile_snapshot(
    config: &MixerConfig,
    controls: &[SourceControls],
    outputs: OutputControls,
) -> Result<ControlSnapshot, ControlMappingError> {
    if controls.len() != config.sources().len() {
        return Err(ControlMappingError::ControlCount {
            expected: config.sources().len(),
            actual: controls.len(),
        });
    }

    let mut snapshot = ControlSnapshot::EMPTY;
    snapshot.outputs = [
        compile_route(outputs.route(MixTarget::Monitor)),
        compile_route(outputs.route(MixTarget::Stream)),
    ];
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
        coefficient: if route.enabled() {
            10.0_f32.powf(route.fader().decibels() / 20.0)
        } else {
            0.0
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use librewave_core::{FaderGain, MixerProfile};

    #[test]
    fn prepared_controls_are_invisible_until_commit_and_drop_releases_the_slot() {
        let profile = MixerProfile::default();
        let controls: Vec<_> = profile.sources.iter().map(|s| s.controls).collect();
        let sources: Vec<_> = controls.iter().map(|s| s.source()).collect();
        let config = MixerConfig::try_new(4, profile.microphone_source, &sources).expect("layout");
        let (mut stager, consumer) = ControlConsumer::channel(config);
        let prepared = stager.try_prepare(&controls, profile.outputs).expect("reserve");
        assert!(consumer.take().is_none());
        assert!(matches!(stager.try_prepare(&controls, profile.outputs), Err(StageError::Full)));
        drop(prepared);
        assert!(consumer.take().is_none());
        let muted: Vec<_> = controls
            .iter()
            .map(|s| {
                SourceControls::new(s.source(), MixRoute::new(false, FaderGain::UNITY), s.stream())
            })
            .collect();
        stager.try_prepare(&muted, profile.outputs).expect("reserve after cancellation").commit();
        let received = consumer.take().expect("committed update");
        assert!(received.monitor.iter().all(|r| r.coefficient == 0.0));
        assert!(consumer.take().is_none());
        stager.try_stage(&controls, profile.outputs).expect("next update");
        assert_eq!(
            consumer.take().expect("next snapshot").monitor[0].coefficient.to_bits(),
            1.0_f32.to_bits()
        );
    }
}
