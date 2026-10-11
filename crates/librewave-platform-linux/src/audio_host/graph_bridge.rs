//! Transfers one validated `PipeWire` graph boundary to the portable clock bridge.

use super::clock_bridge::observation::CopySlotPublisher;
use super::clock_bridge::{
    BridgeError, GraphClockObservation, GraphClockProcessor, GraphOutputBuffers, GraphSystemInput,
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
    fault_reporter: Option<CopySlotPublisher<BridgeError>>,
}

impl ClockBridgeGraphProcessor {
    pub(super) const fn new(epoch: ClockAttemptEpoch, graph: GraphClockProcessor) -> Self {
        Self { epoch, graph, fault_reporter: None }
    }

    pub(super) fn report_faults(mut self, reporter: CopySlotPublisher<BridgeError>) -> Self {
        self.fault_reporter = Some(reporter);
        self
    }

    fn bridge_error(&mut self, error: BridgeError) -> ProcessFault {
        if let Some(reporter) = self.fault_reporter.as_mut() {
            let _ = reporter.try_publish(error);
        }
        ProcessFault::Processor
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
        let silence = self.graph.try_silence_boundary(observation, &mut outputs);
        if silence.map_err(|error| self.bridge_error(error))? {
            return Ok(());
        }
        // A removed link can leave old samples in the adapter's mapped buffer.
        // The drain state is semantic silence from the first detached cycle.
        let system = match block.system_state {
            SystemCycleState::Idle | SystemCycleState::Draining => GraphSystemInput::unconnected(),
            SystemCycleState::Priming | SystemCycleState::Active => {
                GraphSystemInput::connected(block.system.left, block.system.right)
            }
        };
        let result = self.graph.process(observation, system, outputs);
        result.map_err(|error| self.bridge_error(error))?;
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
    use crate::audio_host::clock_bridge::observation::copy_slot;
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
    fn callback_reports_the_exact_bridge_fault_without_logging() {
        let fixture = primed_worker_fixture();
        let epoch = ClockAttemptEpoch::try_new(7).expect("test epoch");
        let (publisher, mut reader) = copy_slot();
        let mut processor =
            ClockBridgeGraphProcessor::new(epoch, fixture.parts.graph).report_faults(publisher);
        let input = vec![0.0; 256];
        let mut microphone_left = vec![0.0; 256];
        let mut microphone_right = vec![0.0; 256];
        let mut monitor_left = vec![0.0; 256];
        let mut monitor_right = vec![0.0; 256];
        let mut stream_left = vec![0.0; 256];
        let mut stream_right = vec![0.0; 256];
        let result = processor.process(ProcessBlock {
            timing: ProcessTiming {
                driver_id: 42,
                rate: 48_000,
                position: fixture.graph_position,
                monotonic_nsec: fixture.graph_time,
                quantum: 2_048,
            },
            system_state: SystemCycleState::Idle,
            system: super::super::pipewire_filter_ffi::StereoInput { left: &input, right: &input },
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
        });
        assert_eq!(result, Err(ProcessFault::Processor));
        assert_eq!(
            reader.try_take(),
            Ok(BridgeError::InvalidQuantum { actual: 2_048, maximum: 1_024 })
        );
    }

    #[test]
    fn detached_system_buffer_is_not_read_during_drain() {
        let mut fixture = primed_worker_fixture();
        fixture.parts.control.activate().expect("activate primed bridge");
        let epoch = ClockAttemptEpoch::try_new(7).expect("test epoch");
        let mut processor = ClockBridgeGraphProcessor::new(epoch, fixture.parts.graph);
        let input = vec![f32::NAN; 256];
        let mut microphone_left = vec![0.0; 256];
        let mut microphone_right = vec![0.0; 256];
        let mut monitor_left = vec![0.0; 256];
        let mut monitor_right = vec![0.0; 256];
        let mut stream_left = vec![0.0; 256];
        let mut stream_right = vec![0.0; 256];
        processor
            .process(ProcessBlock {
                timing: ProcessTiming {
                    driver_id: 42,
                    rate: 48_000,
                    position: fixture.graph_position,
                    monotonic_nsec: fixture.graph_time,
                    quantum: 256,
                },
                system_state: SystemCycleState::Draining,
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
            .expect("drain ignores detached adapter data");
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
