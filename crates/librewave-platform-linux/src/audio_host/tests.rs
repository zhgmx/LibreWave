//! Fake-backed regressions for the production host orchestration path.

use super::alsa::{
    RealAlsaFacade, select_revalidated_card, validate_applied_pcm, validate_applied_software,
    validate_card_id,
};
use super::pipewire::{
    PhysicalObjectKind, PhysicalObjectProperties, RealPipeWireFacade, is_candidate_physical_object,
};
use super::*;
use crate::audio_lifecycle::{AudioLifecycle, DegradedState, LifecycleError, LifecycleState};
use crate::{AlsaCardInfo, DeviceIdentity, UsbTopology};
use std::sync::{Arc, Mutex};

fn card(number: u32, id: &str) -> AlsaCardInfo {
    AlsaCardInfo { number, id: Some(id.to_owned()), name: Some("Wave:3".to_owned()) }
}

fn candidate() -> UsbDeviceCandidate {
    UsbDeviceCandidate {
        identity: DeviceIdentity::wave3(),
        topology: UsbTopology::new("1-8.3"),
        alsa_cards: vec![card(7, "Wave3")],
    }
}

fn pcm_config() -> Wave3PhysicalIoConfig {
    Wave3PhysicalIoConfig::try_new(256, 1_024, 192, 768).expect("valid test PCM geometry")
}

#[test]
fn applied_pcm_validation_has_no_format_or_geometry_fallback() {
    assert_eq!(validate_applied_pcm(PcmDirection::Capture, true, true), Ok(()));
    assert_eq!(
        validate_applied_pcm(PcmDirection::Capture, false, true),
        Err(LinuxAudioError::UnsupportedPcmFormat { direction: PcmDirection::Capture })
    );
    assert_eq!(
        validate_applied_pcm(PcmDirection::Playback, true, false),
        Err(LinuxAudioError::PcmConfigurationAdjusted { direction: PcmDirection::Playback })
    );
}

#[test]
fn exact_card_selection_rejects_renumber_id_topology_and_ambiguity() {
    let expected = candidate();
    assert_eq!(
        select_revalidated_card(&expected, std::slice::from_ref(&expected)),
        Ok((card(7, "Wave3"), UsbTopology::new("1-8.3")))
    );
    for changed in [
        UsbDeviceCandidate { alsa_cards: vec![card(8, "Wave3")], ..expected.clone() },
        UsbDeviceCandidate { alsa_cards: vec![card(7, "Other")], ..expected.clone() },
        UsbDeviceCandidate { topology: UsbTopology::new("1-9"), ..expected.clone() },
    ] {
        assert_eq!(
            select_revalidated_card(&expected, &[changed]),
            Err(LinuxAudioError::AlsaCardChanged)
        );
    }
    let ambiguous = UsbDeviceCandidate {
        alsa_cards: vec![card(7, "Wave3"), card(8, "Wave3_1")],
        ..expected.clone()
    };
    assert_eq!(
        select_revalidated_card(&ambiguous, std::slice::from_ref(&ambiguous)),
        Err(LinuxAudioError::AmbiguousAlsaCard)
    );
}

#[test]
fn alsa_card_id_uses_only_the_kernel_identifier_grammar() {
    for valid in ["Wave3", "Wave_3", "A1"] {
        assert_eq!(validate_card_id(valid), Ok(()));
    }
    for invalid in
        ["", "123", "1Wave", "_Wave", "Wave-3", "Wave3,DEV=9", "Wave3\0Other", "MoreThanFifteen1"]
    {
        assert_eq!(validate_card_id(invalid), Err(LinuxAudioError::InvalidAlsaCardId));
    }

    let expected = UsbDeviceCandidate { alsa_cards: vec![card(7, "Wave3,DEV=9")], ..candidate() };
    assert_eq!(
        select_revalidated_card(&expected, std::slice::from_ref(&expected)),
        Err(LinuxAudioError::InvalidAlsaCardId)
    );
}

#[test]
fn endpoint_allowlist_direction_names_and_properties_are_exact() {
    let plans = endpoint_plans(DELIBERATE_ENDPOINTS).expect("deliberate plans are valid");
    assert_eq!(
        plans,
        vec![
            EndpointPlan {
                endpoint: EndpointId::System,
                direction: PipeWireEndpointDirection::Input,
                node_name: "librewave.system",
                node_description: "LibreWave System",
                media_class: "Audio/Sink",
                node_virtual: true,
            },
            EndpointPlan {
                endpoint: EndpointId::Microphone,
                direction: PipeWireEndpointDirection::Output,
                node_name: "librewave.microphone",
                node_description: "LibreWave Microphone",
                media_class: "Audio/Source",
                node_virtual: true,
            },
            EndpointPlan {
                endpoint: EndpointId::MonitorMix,
                direction: PipeWireEndpointDirection::Output,
                node_name: "librewave.monitor-mix",
                node_description: "LibreWave Monitor Mix",
                media_class: "Audio/Source",
                node_virtual: true,
            },
            EndpointPlan {
                endpoint: EndpointId::StreamMix,
                direction: PipeWireEndpointDirection::Output,
                node_name: "librewave.stream-mix",
                node_description: "LibreWave Stream Mix",
                media_class: "Audio/Source",
                node_virtual: true,
            },
        ]
    );
    assert_eq!(
        endpoint_plans(&[EndpointId::Microphone, EndpointId::Microphone]),
        Err(LinuxAudioError::DuplicateEndpoint(EndpointId::Microphone))
    );
}

#[test]
fn pipewire_physical_objects_match_only_the_admitted_card() {
    let exact_device = PhysicalObjectProperties {
        vendor_id: Some("0x0fd9"),
        product_id: Some("0x0070"),
        alsa_card: Some("2"),
        alsa_path: Some("hw:2"),
    };
    assert!(is_candidate_physical_object(PhysicalObjectKind::Device, exact_device, 2));
    assert!(is_candidate_physical_object(
        PhysicalObjectKind::Node,
        PhysicalObjectProperties { alsa_path: Some("hw:2"), ..PhysicalObjectProperties::default() },
        2
    ));
    assert!(is_candidate_physical_object(
        PhysicalObjectKind::Node,
        PhysicalObjectProperties {
            alsa_path: Some("hw:2,0,0"),
            ..PhysicalObjectProperties::default()
        },
        2
    ));

    for adjacent in [
        PhysicalObjectProperties { alsa_card: Some("20"), ..exact_device },
        PhysicalObjectProperties { product_id: Some("0x0071"), ..exact_device },
    ] {
        assert!(!is_candidate_physical_object(PhysicalObjectKind::Device, adjacent, 2));
    }
    assert!(!is_candidate_physical_object(
        PhysicalObjectKind::Node,
        PhysicalObjectProperties {
            alsa_path: Some("hw:20"),
            ..PhysicalObjectProperties::default()
        },
        2
    ));
    for malformed in ["", "hw:", "hw:02", "hw:2x", "hw:2,", "plughw:2", "hw:2,0,0,0"] {
        assert!(!is_candidate_physical_object(
            PhysicalObjectKind::Node,
            PhysicalObjectProperties {
                alsa_path: Some(malformed),
                ..PhysicalObjectProperties::default()
            },
            2
        ));
    }
    assert!(!is_candidate_physical_object(PhysicalObjectKind::Other, exact_device, 2));
}

#[test]
fn pipewire_scan_fails_closed_without_one_candidate_card() {
    let mut pipewire = RealPipeWireFacade::default();
    for candidate in [
        UsbDeviceCandidate { alsa_cards: Vec::new(), ..candidate() },
        UsbDeviceCandidate {
            alsa_cards: vec![card(2, "Wave3"), card(3, "Wave3_1")],
            ..candidate()
        },
    ] {
        assert_eq!(
            pipewire.verify_physical_nodes_hidden(&candidate),
            Err(LinuxAudioError::AmbiguousAlsaCard)
        );
        assert!(!pipewire.owns_connection());
    }
}

#[test]
fn pcm_config_sizes_independent_checked_period_buffers() {
    let config = pcm_config();
    assert_eq!(config.capture_period_bytes(), Ok(768));
    assert_eq!(config.capture_buffer_bytes(), Ok(3_072));
    assert_eq!(config.playback_period_bytes(), Ok(1_152));
    assert_eq!(config.playback_buffer_bytes(), Ok(4_608));
    assert!(Wave3PhysicalIoConfig::try_new(1, 1, 1, 1).is_ok());
    assert_eq!(
        Wave3PhysicalIoConfig::try_new(2, 1, 1, 1),
        Err(PcmConfigError::PeriodExceedsBuffer { period_frames: 2, buffer_frames: 1 })
    );
}

#[test]
fn applied_software_validation_requires_monotonic_time_and_manual_playback_start() {
    assert_eq!(validate_applied_software(PcmDirection::Capture, true, true), Ok(()));
    assert_eq!(
        validate_applied_software(PcmDirection::Capture, false, true),
        Err(LinuxAudioError::PcmTimestampConfigurationAdjusted {
            direction: PcmDirection::Capture,
        })
    );
    assert_eq!(
        validate_applied_software(PcmDirection::Playback, true, false),
        Err(LinuxAudioError::PlaybackStartThresholdAdjusted)
    );
}

#[test]
fn host_rejects_an_unknown_hidden_node_contract() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut host = LinuxAudioHost::with_facade(Box::new(FakePipeWire {
        calls: Arc::clone(&calls),
        connected: false,
        fail_disconnect: false,
    }));
    assert_eq!(
        host.verify_physical_nodes_hidden(&candidate(), &[HiddenPhysicalNode::Capture]),
        Err(LinuxAudioError::InvalidHiddenPhysicalNodeContract)
    );
    assert!(calls.lock().expect("calls lock").is_empty());
}

#[derive(Debug)]
struct FakePipeWire {
    calls: Arc<Mutex<Vec<&'static str>>>,
    connected: bool,
    fail_disconnect: bool,
}

impl PipeWireFacade for FakePipeWire {
    fn verify_physical_nodes_hidden(
        &mut self,
        _candidate: &UsbDeviceCandidate,
    ) -> Result<(), LinuxAudioError> {
        self.calls.lock().expect("calls lock").push("pipewire-connect");
        self.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), LinuxAudioError> {
        self.calls.lock().expect("calls lock").push("pipewire-disconnect");
        self.connected = false;
        if self.fail_disconnect { Err(LinuxAudioError::TeardownFailed) } else { Ok(()) }
    }

    fn owns_connection(&self) -> bool {
        self.connected
    }
}

#[test]
fn production_host_refuses_before_alsa_until_graph_composition_exists() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let pipewire =
        FakePipeWire { calls: Arc::clone(&calls), connected: false, fail_disconnect: false };
    let mut host = LinuxAudioHost::with_facade(Box::new(pipewire));
    let mut lifecycle = AudioLifecycle::new();
    let error = lifecycle.start(&mut host, Some(&candidate())).expect_err("Stage 2 is not ready");
    assert!(matches!(
        error,
        LifecycleError::Host {
            state: DegradedState::CaptureStartFailed,
            source: LinuxAudioError::EndpointStreamTransportUnavailable,
        }
    ));
    assert_eq!(lifecycle.state(), LifecycleState::Degraded(DegradedState::CaptureStartFailed));
    assert_eq!(
        host.resource_state(),
        AudioResourceState {
            not_ready: Some(GraphNotReady::EndpointStreamTransportUnavailable),
            ..AudioResourceState::default()
        }
    );
    assert_eq!(*calls.lock().expect("calls lock"), ["pipewire-connect", "pipewire-disconnect"]);
}

#[test]
fn endpoint_publication_cannot_bypass_the_stage_two_boundary() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let pipewire =
        FakePipeWire { calls: Arc::clone(&calls), connected: false, fail_disconnect: false };
    let mut host = LinuxAudioHost::with_facade(Box::new(pipewire));
    let device = candidate();
    assert_eq!(
        host.publish_endpoints(&device, DELIBERATE_ENDPOINTS),
        Err(LinuxAudioError::EndpointStreamTransportUnavailable)
    );
    assert!(!calls.lock().expect("calls lock").contains(&"publish"));
}

#[test]
fn facade_has_no_hardware_mixer_control_path() {
    fn assert_audio_only_facade<T: AlsaFacade>() {}
    assert_audio_only_facade::<RealAlsaFacade>();
}
