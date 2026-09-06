//! Safe ALSA card selection, PCM configuration, and direct I/O.

use super::pcm::PhysicalPcmParameters;
use super::{
    AlsaFacade, CapturePcm, CaptureRead, LinuxAudioError, PcmDirection, PlaybackPcm,
    Wave3PhysicalIoConfig,
};
use crate::{
    AlsaCardInfo, DeviceIdentity, DiscoveryPaths, LinuxInventory, UsbDeviceCandidate, UsbTopology,
    WAVE3_USB,
};
use alsa::ctl::{Ctl, DeviceIter};
use alsa::pcm::{Access, Format, HwParams, PCM, State};
use alsa::{Direction, ValueOr};
const ALSA_WAIT_MILLIS: u32 = 100;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SelectedPcmCard {
    pub(super) identity: DeviceIdentity,
    pub(super) topology: UsbTopology,
    pub(super) card_number: u32,
    pub(super) card_id: String,
    pub(super) capture_device: u32,
    pub(super) playback_device: u32,
}

pub(super) fn select_revalidated_card(
    expected: &UsbDeviceCandidate,
    current: &[UsbDeviceCandidate],
) -> Result<(AlsaCardInfo, UsbTopology), LinuxAudioError> {
    let [expected_card] = expected.alsa_cards.as_slice() else {
        return Err(LinuxAudioError::AmbiguousAlsaCard);
    };
    let expected_id = expected_card.id.as_deref().ok_or(LinuxAudioError::MissingAlsaCardId)?;
    validate_card_id(expected_id)?;
    let matches = current
        .iter()
        .filter(|candidate| {
            candidate.identity == expected.identity && candidate.topology == expected.topology
        })
        .collect::<Vec<_>>();
    let [current_candidate] = matches.as_slice() else {
        return Err(LinuxAudioError::AlsaCardChanged);
    };
    let [current_card] = current_candidate.alsa_cards.as_slice() else {
        return Err(LinuxAudioError::AlsaCardChanged);
    };
    if current_card.number != expected_card.number
        || current_card.id.as_deref() != Some(expected_id)
    {
        return Err(LinuxAudioError::AlsaCardChanged);
    }
    Ok(((*current_card).clone(), current_candidate.topology.clone()))
}

pub(super) fn validate_card_id(card_id: &str) -> Result<(), LinuxAudioError> {
    let bytes = card_id.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 15
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1..].iter().any(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
    {
        return Err(LinuxAudioError::InvalidAlsaCardId);
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct RealAlsaFacade {
    config: Wave3PhysicalIoConfig,
    paths: DiscoveryPaths,
}

impl RealAlsaFacade {
    pub(super) fn new(config: Wave3PhysicalIoConfig) -> Self {
        Self { config, paths: DiscoveryPaths::default() }
    }

    fn revalidate(
        &self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<(AlsaCardInfo, UsbTopology), LinuxAudioError> {
        let report = LinuxInventory::new().discover(&self.paths);
        select_revalidated_card(candidate, &report.devices)
    }

    fn enumerate_devices(card_id: &str, direction: Direction) -> Result<Vec<u32>, LinuxAudioError> {
        validate_card_id(card_id)?;
        let ctl = Ctl::new(&format!("hw:CARD={card_id}"), true)
            .map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
        let mut devices = Vec::new();
        for device in DeviceIter::new(&ctl) {
            let Ok(device) = u32::try_from(device) else {
                continue;
            };
            if ctl.pcm_info(device, 0, direction).is_ok() {
                devices.push(device);
            }
        }
        Ok(devices)
    }

    fn revalidate_selection(
        &self,
        candidate: &UsbDeviceCandidate,
        selected: &SelectedPcmCard,
    ) -> Result<(), LinuxAudioError> {
        let (card, topology) = self.revalidate(candidate)?;
        if selected.identity != candidate.identity
            || selected.topology != topology
            || selected.card_number != card.number
            || card.id.as_deref() != Some(selected.card_id.as_str())
        {
            return Err(LinuxAudioError::AlsaCardChanged);
        }
        let capture = Self::enumerate_devices(&selected.card_id, Direction::Capture)?;
        let playback = Self::enumerate_devices(&selected.card_id, Direction::Playback)?;
        if capture.as_slice() != [selected.capture_device]
            || playback.as_slice() != [selected.playback_device]
        {
            return Err(LinuxAudioError::AlsaCardChanged);
        }
        Ok(())
    }

    fn open_pcm(
        &self,
        selected: &SelectedPcmCard,
        device: u32,
        direction: Direction,
    ) -> Result<PCM, LinuxAudioError> {
        validate_card_id(&selected.card_id)?;
        let name = format!("hw:CARD={},DEV={device},SUBDEV=0", selected.card_id);
        let pcm = PCM::new(&name, direction, true)
            .map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
        let info = pcm.info().map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
        if info.get_card() != i32::try_from(selected.card_number).unwrap_or(-1)
            || info.get_device() != device
            || info.get_subdevice() != 0
            || info.get_stream() != direction
        {
            return Err(LinuxAudioError::AlsaCardChanged);
        }
        configure_pcm(
            &pcm,
            self.config.parameters(match direction {
                Direction::Capture => PcmDirection::Capture,
                Direction::Playback => PcmDirection::Playback,
            }),
        )?;
        Ok(pcm)
    }
}

impl AlsaFacade for RealAlsaFacade {
    fn select(
        &mut self,
        candidate: &UsbDeviceCandidate,
    ) -> Result<SelectedPcmCard, LinuxAudioError> {
        if candidate.identity.usb() != WAVE3_USB {
            return Err(LinuxAudioError::AlsaCardChanged);
        }
        let (card, topology) = self.revalidate(candidate)?;
        let card_id = card.id.clone().ok_or(LinuxAudioError::MissingAlsaCardId)?;
        let capture = Self::enumerate_devices(&card_id, Direction::Capture)?;
        let playback = Self::enumerate_devices(&card_id, Direction::Playback)?;
        let [capture_device] = capture.as_slice() else {
            return Err(LinuxAudioError::AmbiguousPcmDirection { direction: "capture" });
        };
        let [playback_device] = playback.as_slice() else {
            return Err(LinuxAudioError::AmbiguousPcmDirection { direction: "playback" });
        };
        Ok(SelectedPcmCard {
            identity: candidate.identity,
            topology,
            card_number: card.number,
            card_id,
            capture_device: *capture_device,
            playback_device: *playback_device,
        })
    }

    fn start_capture(
        &mut self,
        candidate: &UsbDeviceCandidate,
        selected: &SelectedPcmCard,
    ) -> Result<Box<dyn CapturePcm>, LinuxAudioError> {
        self.revalidate_selection(candidate, selected)?;
        let pcm = self.open_pcm(selected, selected.capture_device, Direction::Capture)?;
        pcm.start().map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
        Ok(Box::new(RealCapturePcm { pcm }))
    }

    fn open_playback(
        &mut self,
        candidate: &UsbDeviceCandidate,
        selected: &SelectedPcmCard,
    ) -> Result<Box<dyn PlaybackPcm>, LinuxAudioError> {
        self.revalidate_selection(candidate, selected)?;
        let pcm = self.open_pcm(selected, selected.playback_device, Direction::Playback)?;
        Ok(Box::new(RealPlaybackPcm { pcm }))
    }
}

fn configure_pcm(pcm: &PCM, config: PhysicalPcmParameters) -> Result<(), LinuxAudioError> {
    let format = Format::S243LE;
    let params = HwParams::any(pcm).map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
    params
        .set_access(Access::RWInterleaved)
        .and_then(|()| params.set_format(format))
        .and_then(|()| params.set_channels(config.channels))
        .and_then(|()| params.set_rate(config.rate, ValueOr::Nearest))
        .and_then(|()| params.set_period_size(config.period_frames.into(), ValueOr::Nearest))
        .and_then(|()| params.set_buffer_size(config.buffer_frames.into()))
        .and_then(|()| pcm.hw_params(&params))
        .map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
    let applied =
        pcm.hw_params_current().map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
    let packed_format = applied.get_format() == Ok(format);
    let exact_geometry = applied.get_access() == Ok(Access::RWInterleaved)
        && applied.get_channels() == Ok(config.channels)
        && applied.get_rate() == Ok(config.rate)
        && applied.get_period_size() == Ok(config.period_frames.into())
        && applied.get_buffer_size() == Ok(config.buffer_frames.into());
    validate_applied_pcm(config.direction, packed_format, exact_geometry)?;
    pcm.prepare().map_err(|error| LinuxAudioError::Alsa(error.to_string()))
}

pub(super) fn validate_applied_pcm(
    direction: PcmDirection,
    packed_format: bool,
    exact_geometry: bool,
) -> Result<(), LinuxAudioError> {
    if !packed_format {
        return Err(LinuxAudioError::UnsupportedPcmFormat { direction });
    }
    if !exact_geometry {
        return Err(LinuxAudioError::PcmConfigurationAdjusted { direction });
    }
    Ok(())
}

#[derive(Debug)]
struct RealCapturePcm {
    pcm: PCM,
}

impl CapturePcm for RealCapturePcm {
    fn read_frames(&mut self, bytes: &mut [u8]) -> CaptureRead {
        match self.pcm.wait(Some(ALSA_WAIT_MILLIS)) {
            Ok(false) => CaptureRead::WouldBlock,
            Err(error) => classify_alsa_read_error(&self.pcm, error),
            Ok(true) => match self.pcm.io_bytes().readi(bytes) {
                Ok(frames) => CaptureRead::Frames(u64::try_from(frames).unwrap_or(u64::MAX)),
                Err(error) => classify_alsa_read_error(&self.pcm, error),
            },
        }
    }
}

fn classify_alsa_read_error(pcm: &PCM, error: alsa::Error) -> CaptureRead {
    match error.errno() {
        libc::EAGAIN | libc::EINTR => CaptureRead::WouldBlock,
        libc::EPIPE | libc::ESTRPIPE => match pcm.try_recover(error, true) {
            Ok(()) => finish_alsa_recovery(pcm),
            Err(recovery) if matches!(recovery.errno(), libc::ENODEV | libc::ENXIO) => {
                CaptureRead::Disconnected
            }
            Err(_) => CaptureRead::Failed,
        },
        libc::ENODEV | libc::ENXIO => CaptureRead::Disconnected,
        _ => CaptureRead::Failed,
    }
}

fn finish_alsa_recovery(pcm: &PCM) -> CaptureRead {
    match pcm.state_raw() {
        state if state == State::Prepared as i32 => match pcm.start() {
            Ok(()) => CaptureRead::RecoveredXrun,
            Err(error) if matches!(error.errno(), libc::ENODEV | libc::ENXIO) => {
                CaptureRead::Disconnected
            }
            Err(_) => CaptureRead::Failed,
        },
        state if state == State::Running as i32 => CaptureRead::RecoveredXrun,
        state if state == State::Disconnected as i32 => CaptureRead::Disconnected,
        _ => CaptureRead::Failed,
    }
}

#[derive(Debug)]
struct RealPlaybackPcm {
    pcm: PCM,
}

impl PlaybackPcm for RealPlaybackPcm {
    fn close(&mut self) -> Result<(), LinuxAudioError> {
        self.pcm.drop().map_err(|error| LinuxAudioError::Alsa(error.to_string()))
    }
}
