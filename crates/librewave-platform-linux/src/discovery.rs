//! Read-only USB, sysfs, and ALSA discovery.

use super::{
    AlsaCardInfo, DeviceRegistry, DiscoveryFailure, DiscoveryFailureKind, DiscoveryPaths,
    DiscoveryReport, DiscoverySource, UsbDeviceCandidate, UsbIdentity, UsbTopology,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(super) fn discover_devices(
    registry: &DeviceRegistry,
    paths: &DiscoveryPaths,
) -> DiscoveryReport {
    let mut report = DiscoveryReport::default();
    let entries = match fs::read_dir(&paths.usb_devices) {
        Ok(entries) => entries,
        Err(error) => {
            report.failures.push(failure(
                DiscoverySource::UsbSysfs,
                &paths.usb_devices,
                &error,
                &paths.usb_devices,
            ));
            return report;
        }
    };
    let alsa_cards = read_alsa_cards(paths, &mut report);

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.failures.push(failure(
                    DiscoverySource::UsbSysfs,
                    &paths.usb_devices,
                    &error,
                    &paths.usb_devices,
                ));
                continue;
            }
        };
        let topology_name = entry.file_name().to_string_lossy().into_owned();
        let entry_path = entry.path();
        let Some(vendor) = read_attribute(&entry_path, "idVendor", &paths.usb_devices, &mut report)
        else {
            continue;
        };
        let Some(product) =
            read_attribute(&entry_path, "idProduct", &paths.usb_devices, &mut report)
        else {
            continue;
        };
        let usb = match parse_usb_identity(&vendor, &product) {
            Ok(usb) => usb,
            Err(field) => {
                report.failures.push(DiscoveryFailure {
                    source: DiscoverySource::UsbSysfs,
                    path: relative_path(&paths.usb_devices, &entry_path),
                    kind: DiscoveryFailureKind::InvalidValue { field },
                });
                continue;
            }
        };
        let Some(definition) = registry.find(usb) else {
            continue;
        };
        let topology = UsbTopology::new(&topology_name);
        let alsa_cards = alsa_cards
            .iter()
            .filter(|card| card_matches_usb(card, &entry_path))
            .map(|card| card.info.clone())
            .collect();
        report.devices.push(UsbDeviceCandidate {
            identity: definition.identity(),
            topology,
            alsa_cards,
        });
    }

    report
}

#[derive(Clone, Debug)]
struct AlsaCardObservation {
    info: AlsaCardInfo,
    device_path: PathBuf,
}

fn read_alsa_cards(
    paths: &DiscoveryPaths,
    report: &mut DiscoveryReport,
) -> Vec<AlsaCardObservation> {
    let summary = match fs::read_to_string(&paths.asound_cards) {
        Ok(summary) => summary,
        Err(error) => {
            report.failures.push(failure(
                DiscoverySource::AlsaProcfs,
                &paths.asound_cards,
                &error,
                &paths.asound_cards,
            ));
            String::new()
        }
    };
    let summary_by_number = parse_alsa_summary(&summary);
    let entries = match fs::read_dir(&paths.sound_cards) {
        Ok(entries) => entries,
        Err(error) => {
            report.failures.push(failure(
                DiscoverySource::AlsaSysfs,
                &paths.sound_cards,
                &error,
                &paths.sound_cards,
            ));
            return Vec::new();
        }
    };
    let mut cards = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.failures.push(failure(
                    DiscoverySource::AlsaSysfs,
                    &paths.sound_cards,
                    &error,
                    &paths.sound_cards,
                ));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(number) = name.strip_prefix("card").and_then(|value| value.parse().ok()) else {
            continue;
        };
        let device_link = entry.path().join("device");
        let device_path = match fs::canonicalize(&device_link) {
            Ok(path) => path,
            Err(error) => {
                report.failures.push(failure(
                    DiscoverySource::AlsaSysfs,
                    &device_link,
                    &error,
                    &paths.sound_cards,
                ));
                continue;
            }
        };
        let id_path = paths.asound_root.join(format!("card{number}/id"));
        let id = read_optional_text(&id_path, &paths.asound_root, report);
        let (summary_id, summary_name) =
            summary_by_number.get(&number).cloned().unwrap_or((None, None));
        cards.push(AlsaCardObservation {
            info: AlsaCardInfo { number, id: id.or(summary_id), name: summary_name },
            device_path,
        });
    }
    cards
}

fn parse_alsa_summary(
    summary: &str,
) -> std::collections::BTreeMap<u32, (Option<String>, Option<String>)> {
    let mut result = std::collections::BTreeMap::new();
    for line in summary.lines() {
        let trimmed = line.trim_start();
        let Some((number, rest)) = trimmed.split_once(' ') else {
            continue;
        };
        let Ok(number) = number.parse::<u32>() else {
            continue;
        };
        let Some((bracketed_id, after_id)) = rest.split_once(']') else {
            continue;
        };
        let Some(id) = bracketed_id.strip_prefix('[') else {
            continue;
        };
        let name = after_id.split_once(" - ").map(|(_, name)| name.trim().to_owned());
        result.insert(number, (Some(id.trim().to_owned()), name));
    }
    result
}

fn card_matches_usb(card: &AlsaCardObservation, usb_path: &Path) -> bool {
    let Ok(usb_path) = fs::canonicalize(usb_path) else {
        return false;
    };
    card.device_path.starts_with(usb_path)
}

fn read_attribute(
    entry: &Path,
    name: &str,
    root: &Path,
    report: &mut DiscoveryReport,
) -> Option<String> {
    let path = entry.join(name);
    match fs::read_to_string(&path) {
        Ok(value) => Some(value.trim().to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            report.failures.push(failure(DiscoverySource::UsbSysfs, &path, &error, root));
            None
        }
    }
}

fn read_optional_text(path: &Path, root: &Path, report: &mut DiscoveryReport) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(value) => Some(value.trim().to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            report.failures.push(failure(DiscoverySource::AlsaProcfs, path, &error, root));
            None
        }
    }
}

fn parse_usb_identity(vendor: &str, product: &str) -> Result<UsbIdentity, &'static str> {
    let vendor_id =
        u16::from_str_radix(vendor.trim_start_matches("0x"), 16).map_err(|_| "idVendor")?;
    let product_id =
        u16::from_str_radix(product.trim_start_matches("0x"), 16).map_err(|_| "idProduct")?;
    Ok(UsbIdentity::new(vendor_id, product_id))
}

fn failure(
    source: DiscoverySource,
    path: &Path,
    error: &io::Error,
    root: &Path,
) -> DiscoveryFailure {
    DiscoveryFailure {
        source,
        path: relative_path(root, path),
        kind: match error.kind() {
            io::ErrorKind::PermissionDenied => DiscoveryFailureKind::PermissionDenied,
            io::ErrorKind::NotFound => DiscoveryFailureKind::NotFound,
            kind => DiscoveryFailureKind::Io { kind },
        },
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceDefinition, DeviceModel};
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    const FIXTURE_CARDS: &str = include_str!("../tests/fixtures/proc/asound/cards");
    const FIXTURE_CARD_ID: &str = include_str!("../tests/fixtures/proc/asound/card7/id");
    const FIXTURE_VENDOR: &str = include_str!("../tests/fixtures/sys/device/idVendor");
    const FIXTURE_PRODUCT: &str = include_str!("../tests/fixtures/sys/device/idProduct");

    struct FixtureRoot {
        root: PathBuf,
        paths: DiscoveryPaths,
    }

    impl FixtureRoot {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("librewave-linux-fixture-{nonce}"));
            let usb_target = root.join("sys/devices/pci/usb1/1-8.3");
            let usb_bus = root.join("sys/bus/usb/devices");
            let sound_card = root.join("sys/class/sound/card7");
            let card_target = root.join("sys/devices/pci/usb1/1-8.3/1-8.3:1.0/sound/card7");
            fs::create_dir_all(&usb_target).expect("create USB target");
            fs::create_dir_all(&usb_bus).expect("create USB bus");
            fs::create_dir_all(&sound_card).expect("create sound card");
            fs::create_dir_all(card_target.parent().expect("card target parent"))
                .expect("create card target");
            fs::create_dir_all(&card_target).expect("create card target directory");
            fs::create_dir_all(root.join("proc/asound/card7")).expect("create proc card");
            fs::write(usb_target.join("idVendor"), FIXTURE_VENDOR).expect("write vendor");
            fs::write(usb_target.join("idProduct"), FIXTURE_PRODUCT).expect("write product");
            fs::write(usb_target.join("serial"), "SERIAL-MUST-NOT-LEAK\n")
                .expect("write serial sentinel");
            symlink(&usb_target, usb_bus.join("1-8.3")).expect("link USB bus entry");
            symlink(card_target, sound_card.join("device")).expect("link sound card");
            fs::write(root.join("proc/asound/cards"), FIXTURE_CARDS).expect("write cards");
            fs::write(root.join("proc/asound/card7/id"), FIXTURE_CARD_ID).expect("write card id");
            let paths = DiscoveryPaths {
                usb_devices: usb_bus,
                sound_cards: root.join("sys/class/sound"),
                asound_cards: root.join("proc/asound/cards"),
                asound_root: root.join("proc/asound"),
            };
            Self { root, paths }
        }
    }

    impl Drop for FixtureRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn discovers_wave3_and_correlates_alsa_without_card_assumption() {
        let fixture = FixtureRoot::new();
        let report = discover_devices(&DeviceRegistry::default(), &fixture.paths);
        assert!(report.failures.is_empty(), "failures: {:?}", report.failures);
        assert_eq!(report.devices.len(), 1);
        let device = &report.devices[0];
        assert_eq!(device.identity.usb(), super::super::WAVE3_USB);
        assert_eq!(device.identity.model(), DeviceModel::Wave3);
        assert_eq!(device.topology.as_str(), "1-8.3");
        assert_eq!(device.alsa_cards[0].number, 7);
        assert_eq!(device.alsa_cards[0].id.as_deref(), Some("Wave3"));
        assert_eq!(device.alsa_cards[0].name.as_deref(), Some("Elgato Wave:3"));
    }

    #[test]
    fn candidate_debug_does_not_contain_serial_attribute() {
        let fixture = FixtureRoot::new();
        let report = discover_devices(&DeviceRegistry::default(), &fixture.paths);
        let debug = format!("{report:?}");
        assert!(!debug.contains("SERIAL-MUST-NOT-LEAK"));
    }

    #[test]
    fn registry_admits_only_exact_reviewed_pairs() {
        let registry = DeviceRegistry::default();
        assert_eq!(registry.find(super::super::WAVE3_USB), Some(DeviceDefinition::wave3()));
        assert_eq!(registry.find(UsbIdentity::new(0x0fd9, 0x0071)), None);

        let fixture = FixtureRoot::new();
        fs::write(fixture.paths.usb_devices.join("1-8.3/idProduct"), "0071\n")
            .expect("change product fixture");
        let report = discover_devices(&registry, &fixture.paths);
        assert!(report.devices.is_empty());
        assert!(report.failures.is_empty());
    }

    #[test]
    fn missing_interface_failure_is_explicit() {
        let fixture = FixtureRoot::new();
        let paths = DiscoveryPaths {
            usb_devices: fixture.root.join("missing-usb"),
            ..fixture.paths.clone()
        };
        let report = discover_devices(&DeviceRegistry::default(), &paths);
        assert_eq!(report.devices.len(), 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].kind, DiscoveryFailureKind::NotFound);
    }

    #[test]
    fn permission_failure_kind_is_explicit() {
        let failure = failure(
            DiscoverySource::UsbSysfs,
            Path::new("idVendor"),
            &io::Error::from(io::ErrorKind::PermissionDenied),
            Path::new("."),
        );
        assert_eq!(failure.kind, DiscoveryFailureKind::PermissionDenied);
    }

    #[test]
    fn parses_alsa_summary_fixture() {
        let summary = parse_alsa_summary(FIXTURE_CARDS);
        assert_eq!(summary[&7].0.as_deref(), Some("Wave3"));
        assert_eq!(summary[&7].1.as_deref(), Some("Elgato Wave:3"));
    }
}
