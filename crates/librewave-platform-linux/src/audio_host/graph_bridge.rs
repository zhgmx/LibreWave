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
        let mut outputs = GraphOutputBuffers {
            microphone_left: block.microphone.left,
            microphone_right: block.microphone.right,
            monitor_left: block.monitor.left,
            monitor_right: block.monitor.right,
            stream_left: block.stream.left,
            stream_right: block.stream.right,
        };
        if self
            .graph
            .try_silence_boundary(observation, &mut outputs)
            .map_err(|_| ProcessFault::Processor)?
        {
            return Ok(());
        }
        let system = match block.system_state {
            SystemCycleState::Idle => GraphSystemInput::unconnected(),
            SystemCycleState::Priming | SystemCycleState::Active | SystemCycleState::Draining => {
                GraphSystemInput::connected(block.system.left, block.system.right)
            }
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
    use crate::audio_host::clock_bridge::tests::primed_worker_fixture;

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

    #[test]
    fn pipewire_callback_silences_parked_graph_without_faulting() {
        let mut fixture = primed_worker_fixture();
        let epoch = ClockAttemptEpoch::try_new(7).expect("test epoch");
        let mut processor = ClockBridgeGraphProcessor::new(epoch, fixture.parts.graph);
        let frames = 256;
        let timing = ProcessTiming {
            driver_id: 42,
            rate: 48_000,
            position: fixture.graph_position,
            monotonic_nsec: fixture.graph_time,
            quantum: u32::try_from(frames).expect("test quantum"),
        };
        let input = vec![0.5; frames];
        let mut microphone_left = vec![0.5; frames];
        let mut microphone_right = vec![0.5; frames];
        let mut monitor_left = vec![0.5; frames];
        let mut monitor_right = vec![0.5; frames];
        let mut stream_left = vec![0.5; frames];
        let mut stream_right = vec![0.5; frames];
        processor
            .process(ProcessBlock {
                timing,
                system_state: SystemCycleState::Active,
                system: super::super::pipewire_filter_ffi::StereoInput {
                    left: &input,
                    right: &input,
                },
                microphone: super::super::pipewire_filter_ffi::StereoOutput {
                    left: &mut microphone_left,
                    right: &mut microphone_right,
                },
                monitor: super::super::pipewire_filter_ffi::StereoOutput {
                    left: &mut monitor_left,
                    right: &mut monitor_right,
                },
                stream: super::super::pipewire_filter_ffi::StereoOutput {
                    left: &mut stream_left,
                    right: &mut stream_right,
                },
            })
            .expect("parked callback");
        for output in [
            &microphone_left,
            &microphone_right,
            &monitor_left,
            &monitor_right,
            &stream_left,
            &stream_right,
        ] {
            assert!(output.iter().all(|sample| *sample == 0.0));
        }
        fixture.parts.control.begin_teardown();
        microphone_left.fill(0.5);
        microphone_right.fill(0.5);
        monitor_left.fill(0.5);
        monitor_right.fill(0.5);
        stream_left.fill(0.5);
        stream_right.fill(0.5);
        processor
            .process(ProcessBlock {
                timing: ProcessTiming {
                    position: timing.position + frames as u64,
                    monotonic_nsec: timing.monotonic_nsec + 5_333_333,
                    ..timing
                },
                system_state: SystemCycleState::Idle,
                system: super::super::pipewire_filter_ffi::StereoInput {
                    left: &input,
                    right: &input,
                },
                microphone: super::super::pipewire_filter_ffi::StereoOutput {
                    left: &mut microphone_left,
                    right: &mut microphone_right,
                },
                monitor: super::super::pipewire_filter_ffi::StereoOutput {
                    left: &mut monitor_left,
                    right: &mut monitor_right,
                },
                stream: super::super::pipewire_filter_ffi::StereoOutput {
                    left: &mut stream_left,
                    right: &mut stream_right,
                },
            })
            .expect("quiescing callback");
        for output in [
            &microphone_left,
            &microphone_right,
            &monitor_left,
            &monitor_right,
            &stream_left,
            &stream_right,
        ] {
            assert!(output.iter().all(|sample| *sample == 0.0));
        }
    }
}
