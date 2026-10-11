//! Transfers one validated `PipeWire` graph boundary to the portable clock bridge.

use super::clock_bridge::{
    GraphClockObservation, GraphClockProcessor, GraphOutputBuffers, GraphSystemInput,
};
use super::pipewire_filter_ffi::{
    GraphProcessor, ProcessBlock, ProcessFault, ProcessTiming, SystemCycleState,
};
use librewave_engine::{
    ClockAttemptEpoch, ClockFramePosition, ClockObservation, MonotonicNanoseconds,
};

pub(super) struct ClockBridgeGraphProcessor {
    epoch: ClockAttemptEpoch,
    graph: GraphClockProcessor,
}

impl ClockBridgeGraphProcessor {
    pub(super) const fn new(epoch: ClockAttemptEpoch, graph: GraphClockProcessor) -> Self {
        Self { epoch, graph }
    }
}

impl GraphProcessor for ClockBridgeGraphProcessor {
    fn process(&mut self, block: ProcessBlock<'_>) -> Result<(), ProcessFault> {
        let observation = graph_observation(self.epoch, block.timing)?;
        let system = match block.system_state {
            SystemCycleState::Idle => GraphSystemInput::unconnected(),
            SystemCycleState::Priming | SystemCycleState::Active | SystemCycleState::Draining => {
                GraphSystemInput::connected(block.system.left, block.system.right)
            }
        };
        let outputs = GraphOutputBuffers {
            microphone_left: block.microphone.left,
            microphone_right: block.microphone.right,
            monitor_left: block.monitor.left,
            monitor_right: block.monitor.right,
            stream_left: block.stream.left,
            stream_right: block.stream.right,
        };
        self.graph.process(observation, system, outputs).map_err(|_| ProcessFault::Processor)?;
        Ok(())
    }
}

fn graph_observation(
    epoch: ClockAttemptEpoch,
    timing: ProcessTiming,
) -> Result<GraphClockObservation, ProcessFault> {
    let clock = ClockObservation::new(
        epoch,
        ClockFramePosition::new(timing.position),
        MonotonicNanoseconds::new(timing.monotonic_nsec),
    );
    GraphClockObservation::try_new(clock, timing.quantum as usize)
        .map_err(|_| ProcessFault::InvalidQuantum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_time_uses_the_pipewire_monotonic_sample() {
        let epoch = ClockAttemptEpoch::try_new(7).expect("nonzero epoch");
        let sample = graph_observation(
            epoch,
            ProcessTiming {
                driver_id: 42,
                rate: 48_000,
                position: 512,
                monotonic_nsec: 10_500_000,
                quantum: 128,
            },
        )
        .expect("valid graph clock");
        assert_eq!(sample.clock().epoch(), epoch);
        assert_eq!(sample.clock().frame_position().get(), 512);
        assert_eq!(sample.clock().monotonic_time().get(), 10_500_000);
        assert_eq!(sample.quantum_frames(), 128);
    }
}
