use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RingBuildError {
    ZeroCapacity,
    CapacityOverflow,
    Allocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RingPushError {
    PartialFrame { samples: usize, channels: usize },
    NonFinite { sample_index: usize },
    Overflow { requested: usize, remaining: usize, capacity: usize },
    Sequence(RingSequenceError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RingPeekError {
    PartialFrame { samples: usize, channels: usize },
    Shortage { required: usize, available: usize },
    Sequence(RingSequenceError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RingSequenceError {
    Overflow,
    InvalidOrder,
    CapacityExceeded,
    Conversion,
}

#[derive(Debug)]
pub(super) struct Producer<const CHANNELS: usize> {
    shared: Arc<Ring<CHANNELS>>,
    write_sequence: u64,
}

impl<const CHANNELS: usize> Producer<CHANNELS> {
    #[cfg(test)]
    pub(super) const fn write_sequence(&self) -> u64 {
        self.write_sequence
    }

    pub(super) fn free_frames(&self) -> Result<usize, RingSequenceError> {
        let read_sequence = self.shared.published_read_sequence.load(Ordering::Acquire);
        let fill = self.shared.fill_frames(self.write_sequence, read_sequence)?;
        Ok(self.shared.capacity_frames() - fill)
    }

    pub(super) fn prepare_push(
        &mut self,
        samples: &[f32],
    ) -> Result<PreparedPush<'_, CHANNELS>, RingPushError> {
        if !samples.len().is_multiple_of(CHANNELS) {
            return Err(RingPushError::PartialFrame { samples: samples.len(), channels: CHANNELS });
        }
        if let Some(sample_index) = samples.iter().position(|sample| !sample.is_finite()) {
            return Err(RingPushError::NonFinite { sample_index });
        }
        let frames = samples.len() / CHANNELS;
        let remaining = self.free_frames().map_err(RingPushError::Sequence)?;
        if frames > remaining {
            return Err(RingPushError::Overflow {
                requested: frames,
                remaining,
                capacity: self.shared.capacity_frames(),
            });
        }
        let frame_count = u64::try_from(frames)
            .map_err(|_| RingPushError::Sequence(RingSequenceError::Conversion))?;
        let next_write_sequence = self
            .write_sequence
            .checked_add(frame_count)
            .ok_or(RingPushError::Sequence(RingSequenceError::Overflow))?;
        let mut slot = self.shared.slot(self.write_sequence).map_err(RingPushError::Sequence)?;
        for frame in samples.chunks_exact(CHANNELS) {
            // SAFETY: free-space admission prevents this sole producer from
            // touching unread storage. The write sequence is not published
            // until every sample in the block has been copied.
            unsafe {
                (*self.shared.frames[slot].get()).copy_from_slice(frame);
            }
            slot = self.shared.advance_slot(slot);
        }
        let fill_after_commit = self
            .shared
            .capacity_frames()
            .checked_sub(remaining)
            .and_then(|fill| fill.checked_add(frames))
            .ok_or(RingPushError::Sequence(RingSequenceError::Overflow))?;
        Ok(PreparedPush { producer: self, next_write_sequence, fill_after_commit })
    }
}

/// A copied block whose write sequence has not been published.
///
/// Dropping this value does not publish the copied samples. `commit` is
/// infallible because all cursor arithmetic is checked by `prepare_push`.
pub(super) struct PreparedPush<'a, const CHANNELS: usize> {
    producer: &'a mut Producer<CHANNELS>,
    next_write_sequence: u64,
    fill_after_commit: usize,
}

impl<const CHANNELS: usize> PreparedPush<'_, CHANNELS> {
    pub(super) const fn fill_after_commit(&self) -> usize {
        self.fill_after_commit
    }

    pub(super) fn commit(self) {
        self.producer.write_sequence = self.next_write_sequence;
        self.producer
            .shared
            .published_write_sequence
            .store(self.next_write_sequence, Ordering::Release);
    }
}

#[derive(Debug)]
pub(super) struct Consumer<const CHANNELS: usize> {
    shared: Arc<Ring<CHANNELS>>,
    read_sequence: u64,
}

impl<const CHANNELS: usize> Consumer<CHANNELS> {
    #[cfg(test)]
    pub(super) const fn read_sequence(&self) -> u64 {
        self.read_sequence
    }

    pub(super) fn available_frames(&self) -> Result<usize, RingSequenceError> {
        let write_sequence = self.shared.published_write_sequence.load(Ordering::Acquire);
        self.shared.fill_frames(write_sequence, self.read_sequence)
    }

    pub(super) fn peek_exact(
        &mut self,
        output: &mut [f32],
    ) -> Result<PreparedRead<'_, CHANNELS>, RingPeekError> {
        if !output.len().is_multiple_of(CHANNELS) {
            return Err(RingPeekError::PartialFrame { samples: output.len(), channels: CHANNELS });
        }
        let frames = output.len() / CHANNELS;
        let available = self.available_frames().map_err(RingPeekError::Sequence)?;
        if frames > available {
            return Err(RingPeekError::Shortage { required: frames, available });
        }
        let mut slot = self.shared.slot(self.read_sequence).map_err(RingPeekError::Sequence)?;
        for frame in output.chunks_exact_mut(CHANNELS) {
            // SAFETY: the acquired write sequence proves that each copied
            // slot is fully published. The consumer does not release any slot
            // until commit publishes the advanced read sequence.
            let source = unsafe { &*self.shared.frames[slot].get() };
            frame.copy_from_slice(source);
            slot = self.shared.advance_slot(slot);
        }
        let frame_count = u64::try_from(frames)
            .map_err(|_| RingPeekError::Sequence(RingSequenceError::Conversion))?;
        let next_read_sequence = self
            .read_sequence
            .checked_add(frame_count)
            .ok_or(RingPeekError::Sequence(RingSequenceError::Overflow))?;
        Ok(PreparedRead { consumer: self, next_read_sequence })
    }
}

/// A copied block whose read sequence has not been published.
///
/// Dropping this value leaves every frame available to the consumer. `commit`
/// is infallible because all cursor arithmetic is checked by `peek_exact`.
pub(super) struct PreparedRead<'a, const CHANNELS: usize> {
    consumer: &'a mut Consumer<CHANNELS>,
    next_read_sequence: u64,
}

impl<const CHANNELS: usize> PreparedRead<'_, CHANNELS> {
    pub(super) fn commit(self) {
        self.consumer.read_sequence = self.next_read_sequence;
        self.consumer
            .shared
            .published_read_sequence
            .store(self.next_read_sequence, Ordering::Release);
    }
}

/// Read-only access to the ring's authoritative published sequences.
#[derive(Clone, Debug)]
pub(super) struct FillObserver<const CHANNELS: usize> {
    shared: Arc<Ring<CHANNELS>>,
}

impl<const CHANNELS: usize> FillObserver<CHANNELS> {
    /// Reads both published sequences. The caller must prevent either owner
    /// from publishing while it uses this value for a state transition.
    pub(super) fn stable_fill_frames(&self) -> Result<usize, RingSequenceError> {
        let read_sequence = self.shared.published_read_sequence.load(Ordering::Acquire);
        let write_sequence = self.shared.published_write_sequence.load(Ordering::Acquire);
        self.shared.fill_frames(write_sequence, read_sequence)
    }
}

pub(super) fn channel<const CHANNELS: usize>(
    capacity_frames: usize,
) -> Result<(Producer<CHANNELS>, Consumer<CHANNELS>, FillObserver<CHANNELS>), RingBuildError> {
    if capacity_frames == 0 {
        return Err(RingBuildError::ZeroCapacity);
    }
    let capacity_sequence =
        u64::try_from(capacity_frames).map_err(|_| RingBuildError::CapacityOverflow)?;
    let mut frames = Vec::new();
    frames.try_reserve_exact(capacity_frames).map_err(|_| RingBuildError::Allocation)?;
    for _ in 0..capacity_frames {
        frames.push(UnsafeCell::new([0.0; CHANNELS]));
    }
    let shared = Arc::new(Ring {
        frames,
        capacity_sequence,
        published_write_sequence: AtomicU64::new(0),
        published_read_sequence: AtomicU64::new(0),
    });
    Ok((
        Producer { shared: Arc::clone(&shared), write_sequence: 0 },
        Consumer { shared: Arc::clone(&shared), read_sequence: 0 },
        FillObserver { shared },
    ))
}

#[derive(Debug)]
struct Ring<const CHANNELS: usize> {
    frames: Vec<UnsafeCell<[f32; CHANNELS]>>,
    capacity_sequence: u64,
    published_write_sequence: AtomicU64,
    published_read_sequence: AtomicU64,
}

impl<const CHANNELS: usize> Ring<CHANNELS> {
    fn capacity_frames(&self) -> usize {
        self.frames.len()
    }

    fn advance_slot(&self, index: usize) -> usize {
        if index + 1 == self.frames.len() { 0 } else { index + 1 }
    }

    fn slot(&self, sequence: u64) -> Result<usize, RingSequenceError> {
        usize::try_from(sequence % self.capacity_sequence)
            .map_err(|_| RingSequenceError::Conversion)
    }

    fn fill_frames(
        &self,
        write_sequence: u64,
        read_sequence: u64,
    ) -> Result<usize, RingSequenceError> {
        let fill =
            write_sequence.checked_sub(read_sequence).ok_or(RingSequenceError::InvalidOrder)?;
        if fill > self.capacity_sequence {
            return Err(RingSequenceError::CapacityExceeded);
        }
        usize::try_from(fill).map_err(|_| RingSequenceError::Conversion)
    }
}

// SAFETY: each ring has one producer and one consumer. Release/acquire cursor
// publication transfers slot ownership only after a complete block is copied
// or consumed. The monotonic sequences are the only fill representation.
unsafe impl<const CHANNELS: usize> Sync for Ring<CHANNELS> {}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    #[test]
    fn publish_is_whole_and_peek_is_transactional() {
        let (mut producer, mut consumer, observer) = channel::<2>(3).expect("test ring");
        let prepared = producer.prepare_push(&[1.0, 2.0, 3.0, 4.0]).expect("prepare");
        assert_eq!(prepared.fill_after_commit(), 2);
        prepared.commit();
        let mut first = [0.0; 2];
        let prepared = consumer.peek_exact(&mut first).expect("peek");
        assert_eq!(first, [1.0, 2.0]);
        assert_eq!(producer.free_frames(), Ok(1));
        assert_eq!(observer.stable_fill_frames(), Ok(2));
        prepared.commit();
        assert_eq!(consumer.available_frames(), Ok(1));
        assert_eq!(producer.free_frames(), Ok(2));
        assert_eq!(observer.stable_fill_frames(), Ok(1));
    }

    #[test]
    fn rejected_publish_exposes_no_partial_block() {
        let (mut producer, consumer, observer) = channel::<2>(2).expect("test ring");
        assert!(matches!(
            producer.prepare_push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            Err(RingPushError::Overflow { .. })
        ));
        assert_eq!(consumer.available_frames(), Ok(0));
        assert!(matches!(
            producer.prepare_push(&[1.0, f32::NAN]),
            Err(RingPushError::NonFinite { sample_index: 1 })
        ));
        assert_eq!(consumer.available_frames(), Ok(0));
        assert_eq!(observer.stable_fill_frames(), Ok(0));
    }

    #[test]
    fn wrapped_blocks_keep_exact_sample_order() {
        let (mut producer, mut consumer, _observer) = channel::<1>(4).expect("test ring");
        producer.prepare_push(&[1.0, 2.0, 3.0]).expect("first publish").commit();
        let mut first = [0.0; 2];
        consumer.peek_exact(&mut first).expect("first peek").commit();
        producer.prepare_push(&[4.0, 5.0, 6.0]).expect("wrapped publish").commit();
        let mut wrapped = [0.0; 4];
        consumer.peek_exact(&mut wrapped).expect("wrapped peek");
        assert_eq!(wrapped, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn interleaved_producer_and_consumer_completions_keep_exact_fill() {
        let (mut producer, mut consumer, observer) = channel::<1>(6).expect("test ring");
        producer.prepare_push(&[1.0, 2.0, 3.0]).expect("initial publish").commit();
        let mut pending = [0.0; 2];
        let pending_read = consumer.peek_exact(&mut pending).expect("transactional peek");

        producer.prepare_push(&[4.0, 5.0]).expect("concurrent-side publish").commit();
        pending_read.commit();

        assert_eq!(observer.stable_fill_frames(), Ok(3));
        assert_eq!(consumer.available_frames(), Ok(3));
        let mut remaining = [0.0; 3];
        consumer.peek_exact(&mut remaining).expect("remaining samples");
        assert_eq!(remaining, [3.0, 4.0, 5.0]);
    }

    #[test]
    fn absolute_write_sequence_overflow_is_rejected() {
        let (mut producer, _consumer, _observer) = channel::<1>(2).expect("test ring");
        producer.write_sequence = u64::MAX;
        producer.shared.published_write_sequence.store(u64::MAX, Ordering::Release);
        producer.shared.published_read_sequence.store(u64::MAX, Ordering::Release);

        assert!(matches!(
            producer.prepare_push(&[1.0]),
            Err(RingPushError::Sequence(RingSequenceError::Overflow))
        ));
        assert_eq!(producer.write_sequence, u64::MAX);
        assert_eq!(producer.shared.published_write_sequence.load(Ordering::Acquire), u64::MAX);
    }

    #[test]
    #[allow(clippy::drop_non_drop)]
    fn dropped_preparations_publish_no_sequence() {
        let (mut producer, mut consumer, observer) = channel::<1>(2).expect("test ring");
        drop(producer.prepare_push(&[1.0]).expect("prepared push"));
        assert_eq!(observer.stable_fill_frames(), Ok(0));

        producer.prepare_push(&[2.0]).expect("published push").commit();
        let mut sample = [0.0];
        drop(consumer.peek_exact(&mut sample).expect("prepared read"));
        assert_eq!(sample, [2.0]);
        assert_eq!(observer.stable_fill_frames(), Ok(1));
    }
}
