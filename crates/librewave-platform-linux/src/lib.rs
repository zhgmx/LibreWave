//! Linux USB control, discovery, and host-audio ownership.
//!
//! This crate can inspect the reviewed Wave:3 normal-mode USB device or open one
//! exclusive vendor-control connection. The connection admits reads and typed,
//! reversible writes through one handle. It does not claim audio interfaces,
//! detach kernel drivers, change ALSA controls, or modify `WirePlumber`, udev,
//! or systemd state. The audio host opens only a revalidated physical PCM. It
//! does not publish product endpoints until a portable engine can supply them.

use std::io;
use std::path::PathBuf;

pub use librewave_device::{DeviceIdentity, DeviceModel, UsbIdentity};

pub mod audio_host;
pub mod audio_lifecycle;
pub mod audio_policy;
mod discovery;
mod services;
mod usb;

pub use usb::{
    DescriptorError, TopologyError, UsbProbeError, Wave3SnapshotError, Wave3UsbConnection,
    admission_error, admission_snapshot, inspect_wave3_usb, probe_wave3_usb, validate_usb_topology,
    wave3_config_snapshot,
};

pub mod ipc;

/// The Wave:3's reviewed normal-mode USB identity.
pub const WAVE3_USB: UsbIdentity = DeviceIdentity::wave3().usb();

/// Filesystem locations used by Linux discovery.
#[derive(Clone, Debug)]
pub struct DiscoveryPaths {
    /// USB device entries, normally `/sys/bus/usb/devices`.
    pub usb_devices: PathBuf,
    /// ALSA card symlinks, normally `/sys/class/sound`.
    pub sound_cards: PathBuf,
    /// ALSA card summary, normally `/proc/asound/cards`.
    pub asound_cards: PathBuf,
    /// ALSA card directory, normally `/proc/asound`.
    pub asound_root: PathBuf,
}

impl Default for DiscoveryPaths {
    fn default() -> Self {
        Self {
            usb_devices: PathBuf::from("/sys/bus/usb/devices"),
            sound_cards: PathBuf::from("/sys/class/sound"),
            asound_cards: PathBuf::from("/proc/asound/cards"),
            asound_root: PathBuf::from("/proc/asound"),
        }
    }
}

/// A product entry in the extensible platform admission registry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeviceDefinition {
    /// The sanitized identity produced when this USB pair is found.
    identity: DeviceIdentity,
}

impl DeviceDefinition {
    /// The first admitted Linux product.
    #[must_use]
    pub const fn wave3() -> Self {
        Self { identity: DeviceIdentity::wave3() }
    }

    /// Returns the reviewed identity for this definition.
    #[must_use]
    pub const fn identity(&self) -> DeviceIdentity {
        self.identity
    }
}

/// An extensible registry of exact, admitted USB product pairs.
#[derive(Clone, Debug)]
pub struct DeviceRegistry {
    definitions: Vec<DeviceDefinition>,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self { definitions: vec![DeviceDefinition::wave3()] }
    }
}

impl DeviceRegistry {
    /// Creates a registry from exact product definitions.
    #[must_use]
    pub fn new(definitions: impl IntoIterator<Item = DeviceDefinition>) -> Self {
        Self { definitions: definitions.into_iter().collect() }
    }

    /// Adds another exact product definition.
    pub fn register(&mut self, definition: DeviceDefinition) {
        if !self.definitions.iter().any(|known| known == &definition) {
            self.definitions.push(definition);
        }
    }

    fn find(&self, usb: UsbIdentity) -> Option<DeviceDefinition> {
        self.definitions.iter().copied().find(|definition| definition.identity().usb() == usb)
    }
}

/// A topology-only name from `/sys/bus/usb/devices`.
///
/// It is not a USB serial number. The adapter reads no `serial` sysfs
/// attribute. The name is retained only to correlate the USB device with its
/// ALSA card during this inventory pass.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UsbTopology {
    name: String,
}

impl UsbTopology {
    pub(crate) fn new(name: &str) -> Self {
        Self { name: name.to_owned() }
    }

    /// Returns the kernel topology name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

/// Information for one ALSA card associated with a USB candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlsaCardInfo {
    /// The kernel-assigned card number. It is discovered, not configured.
    pub number: u32,
    /// The short ALSA card identifier, when readable.
    pub id: Option<String>,
    /// The human-readable name from `/proc/asound/cards`, when readable.
    pub name: Option<String>,
}

/// A supported USB device and its currently associated host-audio cards.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsbDeviceCandidate {
    /// The portable identity. It never contains a USB serial number.
    pub identity: DeviceIdentity,
    /// The sysfs topology name used for this inventory correlation.
    pub topology: UsbTopology,
    /// ALSA cards whose sysfs device is below this USB device.
    pub alsa_cards: Vec<AlsaCardInfo>,
}

/// The source of a discovery observation or failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoverySource {
    /// USB sysfs entries.
    UsbSysfs,
    /// ALSA sysfs card links.
    AlsaSysfs,
    /// ALSA procfs inventory.
    AlsaProcfs,
}

/// A classified failure from a read-only discovery operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryFailure {
    /// The Linux interface that could not be read or parsed.
    pub source: DiscoverySource,
    /// A path relative to the configured interface root where possible.
    pub path: String,
    /// The explicit failure class.
    pub kind: DiscoveryFailureKind,
}

/// Failure classes are kept separate so callers can distinguish permissions
/// from absence and malformed host data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryFailureKind {
    /// The process was denied access to the interface.
    PermissionDenied,
    /// The interface or attribute does not exist.
    NotFound,
    /// The kernel returned another I/O error.
    Io { kind: io::ErrorKind },
    /// A readable attribute did not match the expected format.
    InvalidValue { field: &'static str },
}

/// Results from a filesystem discovery pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryReport {
    /// Supported devices found in sysfs.
    pub devices: Vec<UsbDeviceCandidate>,
    /// Non-fatal and root-level failures encountered while reading interfaces.
    pub failures: Vec<DiscoveryFailure>,
}

/// Current status of one host service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceAvailability {
    /// The status probe succeeded.
    Available,
    /// The status probe gave an explicit unavailable result.
    Unavailable(ServiceFailure),
}

/// Reasons a host-service status cannot be reported as available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceFailure {
    /// The status executable is not installed.
    NotInstalled,
    /// The process could not access the user/session service.
    PermissionDenied,
    /// The service is installed but not currently reachable or active.
    NotRunning,
    /// The probe returned an unexpected failure.
    ProbeFailed,
}

/// Read-only status of the two host audio services.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostAudioServices {
    /// `PipeWire` registry connectivity from `pw-cli info 0`.
    pub pipewire: ServiceAvailability,
    /// `WirePlumber` user-service state from `systemctl --user is-active`.
    pub wireplumber: ServiceAvailability,
}

/// Combined read-only inventory for the Linux host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostAudioInventory {
    /// Supported USB devices and associated ALSA cards.
    pub discovery: DiscoveryReport,
    /// Current `PipeWire` and `WirePlumber` status.
    pub services: HostAudioServices,
}

/// Performs read-only Linux discovery with a fixed admission registry.
#[derive(Clone, Debug, Default)]
pub struct LinuxInventory {
    registry: DeviceRegistry,
}

impl LinuxInventory {
    /// Creates an inventory reader using the default Wave:3 registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an inventory reader with an explicit, extensible registry.
    #[must_use]
    pub fn with_registry(registry: DeviceRegistry) -> Self {
        Self { registry }
    }

    /// Reads USB sysfs and ALSA procfs/sysfs without opening hardware.
    #[must_use]
    pub fn discover(&self, paths: &DiscoveryPaths) -> DiscoveryReport {
        discovery::discover_devices(&self.registry, paths)
    }

    /// Reads the current host-service state using status-only commands.
    #[must_use]
    pub fn services(&self) -> HostAudioServices {
        services::probe_services()
    }

    /// Reads the complete host inventory without hardware or session writes.
    #[must_use]
    pub fn inspect(&self, paths: &DiscoveryPaths) -> HostAudioInventory {
        HostAudioInventory { discovery: self.discover(paths), services: self.services() }
    }
}
