//! Keeps other ALSA writers away from the Wave:3 level controls.
//!
//! `alsactl` writes its saved card volumes when the card appears. Those values
//! are often stale, because ALSA does not see a change made with the dial.
//! While the daemon holds an [`AlsaLevelLock`], such a write fails, so the
//! daemon's own level restore is the last write. The lock only blocks writes
//! through ALSA. It never writes a control itself.

use crate::{DiscoveryPaths, LinuxInventory, UsbDeviceCandidate};
use alsa::Ctl;
use alsa::ctl::{ElemId, ElemIface};
use std::fmt;
use std::io;
use std::path::Path;

/// The ALSA mixer controls of the Wave:3 headphone level and mute and the
/// microphone gain and mute.
pub const WAVE3_LEVEL_CONTROLS: [&std::ffi::CStr; 4] =
    [c"PCM Playback Volume", c"PCM Playback Switch", c"Mic Capture Volume", c"Mic Capture Switch"];

/// The level controls of one card, locked against writes from other ALSA
/// clients. Dropping the value closes the control device, which releases the
/// locks.
pub struct AlsaLevelLock {
    _ctl: Ctl,
}

impl fmt::Debug for AlsaLevelLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AlsaLevelLock")
    }
}

/// Why the level controls are not locked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AlsaLevelLockError {
    /// The device has no ALSA card yet.
    NoCard,
    /// The device has more than one ALSA card.
    AmbiguousCard,
    /// The card at the discovered number is not the discovered card.
    CardChanged,
    /// ALSA refused to open the card or to lock a control.
    Alsa(String),
}

impl fmt::Display for AlsaLevelLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCard => formatter.write_str("the device has no ALSA card yet"),
            Self::AmbiguousCard => formatter.write_str("the device has more than one ALSA card"),
            Self::CardChanged => formatter.write_str("the ALSA card changed while it was opened"),
            Self::Alsa(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AlsaLevelLockError {}

impl AlsaLevelLock {
    /// Locks the level controls on the current ALSA card of `candidate`.
    ///
    /// # Errors
    ///
    /// Returns an error when the device has no single card, the card changes
    /// while it is opened, a control is missing, or another client holds a lock.
    pub fn acquire(
        paths: &DiscoveryPaths,
        candidate: &UsbDeviceCandidate,
    ) -> Result<Self, AlsaLevelLockError> {
        let report = LinuxInventory::new().discover(paths);
        let current = report
            .devices
            .iter()
            .find(|current| {
                current.identity == candidate.identity && current.topology == candidate.topology
            })
            .ok_or(AlsaLevelLockError::NoCard)?;
        let card = match current.alsa_cards.as_slice() {
            [] => return Err(AlsaLevelLockError::NoCard),
            [card] => card,
            _ => return Err(AlsaLevelLockError::AmbiguousCard),
        };
        let alsa = |error: alsa::Error| AlsaLevelLockError::Alsa(error.to_string());
        let ctl = Ctl::new(&format!("hw:{}", card.number), false).map_err(alsa)?;
        let info = ctl.card_info().map_err(alsa)?;
        if u32::try_from(info.get_card().get_index()).ok() != Some(card.number)
            || card.id.as_deref().is_none_or(|id| info.get_id().ok() != Some(id))
        {
            return Err(AlsaLevelLockError::CardChanged);
        }
        for name in WAVE3_LEVEL_CONTROLS {
            let mut id = ElemId::new(ElemIface::Mixer);
            id.set_name(name);
            ctl.elem_lock(&id).map_err(|error| {
                AlsaLevelLockError::Alsa(format!(
                    "could not lock the ALSA control `{}`: {error}",
                    name.to_string_lossy()
                ))
            })?;
        }
        Ok(Self { _ctl: ctl })
    }
}

/// Names the current USB connection of a device: the boot and the bus and
/// device numbers. A replug or a reboot gives a new name. A daemon restart
/// keeps it.
///
/// # Errors
///
/// Returns an error when the boot ID or the USB numbers cannot be read.
pub fn usb_connection_id(
    paths: &DiscoveryPaths,
    candidate: &UsbDeviceCandidate,
) -> io::Result<String> {
    connection_id(
        Path::new("/proc/sys/kernel/random/boot_id"),
        &paths.usb_devices.join(candidate.topology.as_str()),
    )
}

fn connection_id(boot_id: &Path, device: &Path) -> io::Result<String> {
    let boot = std::fs::read_to_string(boot_id)?;
    let number = |name: &str| -> io::Result<u16> {
        std::fs::read_to_string(device.join(name))?.trim().parse().map_err(io::Error::other)
    };
    let boot = boot.trim();
    if boot.len() != 36 || !boot.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the boot ID is invalid"));
    }
    Ok(format!("{boot}/{:03}/{:03}", number("busnum")?, number("devnum")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn the_connection_id_names_the_boot_and_the_usb_numbers() {
        let root = std::env::temp_dir().join(format!("librewave-levels-{}", std::process::id()));
        let device = root.join("1-8.3");
        fs::create_dir_all(&device).expect("fixture");
        let boot = root.join("boot_id");
        fs::write(&boot, "0f3a6c1e-2b4d-4e5f-8a9b-0c1d2e3f4a5b\n").expect("boot");
        fs::write(device.join("busnum"), "1\n").expect("bus");
        fs::write(device.join("devnum"), "19\n").expect("dev");
        assert_eq!(
            connection_id(&boot, &device).expect("id"),
            "0f3a6c1e-2b4d-4e5f-8a9b-0c1d2e3f4a5b/001/019"
        );
        fs::write(&boot, "not a boot id").expect("boot");
        assert!(connection_id(&boot, &device).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
