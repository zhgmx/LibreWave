//! Safe ALSA card selection, PCM configuration, and direct I/O.

use super::pcm::PhysicalPcmParameters;
use super::worker::{CapturePcm, PcmIoError, PcmStatusSnapshot, PcmWait, PlaybackPcm};
use super::{AlsaFacade, LinuxAudioError, PcmDirection, Wave3PhysicalIoConfig};
use crate::{
    AlsaCardInfo, DeviceIdentity, DiscoveryPaths, LinuxInventory, UsbDeviceCandidate, UsbTopology,
    WAVE3_USB,
};
use alsa::ctl::{Ctl, DeviceIter};
use alsa::pcm::{Access, Format, HwParams, PCM, TstampType};
use alsa::{Direction, ValueOr};

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

    fn open_capture(
        &mut self,
        candidate: &UsbDeviceCandidate,
        selected: &SelectedPcmCard,
    ) -> Result<Box<dyn CapturePcm>, LinuxAudioError> {
        self.revalidate_selection(candidate, selected)?;
        let pcm = self.open_pcm(selected, selected.capture_device, Direction::Capture)?;
        Ok(Box::new(RealCapturePcm {
            pcm,
            geometry: self.config.parameters(PcmDirection::Capture),
        }))
    }

    fn open_playback(
        &mut self,
        candidate: &UsbDeviceCandidate,
        selected: &SelectedPcmCard,
    ) -> Result<Box<dyn PlaybackPcm>, LinuxAudioError> {
        self.revalidate_selection(candidate, selected)?;
        let pcm = self.open_pcm(selected, selected.playback_device, Direction::Playback)?;
        Ok(Box::new(RealPlaybackPcm {
            pcm,
            geometry: self.config.parameters(PcmDirection::Playback),
        }))
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
    let software =
        pcm.sw_params_current().map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
    software
        .set_tstamp_mode(true)
        .and_then(|()| software.set_tstamp_type(TstampType::Monotonic))
        .map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
    let playback_start_threshold = if config.direction == PcmDirection::Playback {
        let boundary =
            software.get_boundary().map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
        software
            .set_start_threshold(boundary)
            .map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
        Some(boundary)
    } else {
        None
    };
    pcm.sw_params(&software).map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
    let applied_software =
        pcm.sw_params_current().map_err(|error| LinuxAudioError::Alsa(error.to_string()))?;
    let timestamp_exact = applied_software.get_tstamp_mode() == Ok(true)
        && applied_software.get_tstamp_type() == Ok(TstampType::Monotonic);
    let start_threshold_exact = playback_start_threshold
        .is_none_or(|expected| applied_software.get_start_threshold() == Ok(expected));
    validate_applied_software(config.direction, timestamp_exact, start_threshold_exact)?;
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

pub(super) fn validate_applied_software(
    direction: PcmDirection,
    timestamp_exact: bool,
    start_threshold_exact: bool,
) -> Result<(), LinuxAudioError> {
    if !timestamp_exact {
        return Err(LinuxAudioError::PcmTimestampConfigurationAdjusted { direction });
    }
    if !start_threshold_exact {
        return Err(LinuxAudioError::PlaybackStartThresholdAdjusted);
    }
    Ok(())
}

#[derive(Debug)]
struct RealCapturePcm {
    pcm: PCM,
    geometry: PhysicalPcmParameters,
}

impl CapturePcm for RealCapturePcm {
    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        self.pcm.start().map_err(classify_alsa_error)
    }

    fn wait(&mut self, timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        self.pcm
            .wait(Some(timeout_millis))
            .map(|ready| if ready { PcmWait::Ready } else { PcmWait::TimedOut })
            .map_err(classify_alsa_error)
    }

    fn read_frames(&mut self, bytes: &mut [u8]) -> Result<usize, PcmIoError> {
        self.pcm.io_bytes().readi(bytes).map_err(classify_alsa_error)
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        pcm_status(&self.pcm, self.geometry)
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        self.pcm.drop().map_err(classify_alsa_error)
    }
}

#[derive(Debug)]
struct RealPlaybackPcm {
    pcm: PCM,
    geometry: PhysicalPcmParameters,
}

impl PlaybackPcm for RealPlaybackPcm {
    fn wait(&mut self, timeout_millis: u32) -> Result<PcmWait, PcmIoError> {
        self.pcm
            .wait(Some(timeout_millis))
            .map(|ready| if ready { PcmWait::Ready } else { PcmWait::TimedOut })
            .map_err(classify_alsa_error)
    }

    fn write_frames(&mut self, bytes: &[u8]) -> Result<usize, PcmIoError> {
        self.pcm.io_bytes().writei(bytes).map_err(classify_alsa_error)
    }

    fn status(&mut self) -> Result<PcmStatusSnapshot, PcmIoError> {
        pcm_status(&self.pcm, self.geometry)
    }

    fn start_pcm(&mut self) -> Result<(), PcmIoError> {
        self.pcm.start().map_err(classify_alsa_error)
    }

    fn drop_stream(&mut self) -> Result<(), PcmIoError> {
        self.pcm.drop().map_err(classify_alsa_error)
    }
}

fn pcm_status(pcm: &PCM, geometry: PhysicalPcmParameters) -> Result<PcmStatusSnapshot, PcmIoError> {
    let status = pcm.status().map_err(classify_alsa_error)?;
    let timestamp = status.get_htstamp();
    let available_frames = status.get_avail();
    let delay_frames = status.get_delay();
    // alsa 0.12.1 unwraps inside Status::get_state. The separate raw state
    // sample keeps invalid state values on the checked error path. ALSA can
    // advance between these calls, so the state gate has a small sampling skew.
    let state_raw = pcm.state_raw();
    Ok(PcmStatusSnapshot {
        timestamp_seconds: checked_i64(timestamp.tv_sec)?,
        timestamp_nanoseconds: checked_i64(timestamp.tv_nsec)?,
        available_frames: checked_i64(available_frames)?,
        delay_frames: checked_i64(delay_frames)?,
        state_raw,
        geometry,
    })
}

fn checked_i64<T>(value: T) -> Result<i64, PcmIoError>
where
    i64: TryFrom<T>,
{
    i64::try_from(value).map_err(|_| PcmIoError::Failed { errno: libc::EOVERFLOW })
}

fn classify_alsa_error(error: alsa::Error) -> PcmIoError {
    match error.errno() {
        libc::EAGAIN => PcmIoError::Again,
        libc::EINTR => PcmIoError::Interrupted,
        libc::EPIPE => PcmIoError::Xrun,
        libc::ESTRPIPE => PcmIoError::Suspended,
        libc::ENODEV | libc::ENXIO => PcmIoError::Disconnected,
        errno => PcmIoError::Failed { errno },
    }
}
