//! Exact, render-only Linux ownership policy for the admitted Wave:3.
//!
//! This module describes files that setup may install and the ownership plan
//! that a future ALSA and `PipeWire` adapter must implement. Rendering a policy
//! does not inspect or change the host.

use crate::{DeviceIdentity, UsbIdentity};
use librewave_core::{DELIBERATE_ENDPOINTS, EndpointId};

/// The `WirePlumber` 0.5 fragment installed in the user's configuration.
pub const WIREPLUMBER_POLICY_FILE_NAME: &str = "80-librewave-wave3.conf";

/// The udev rule installed in the system rules directory.
pub const UDEV_POLICY_FILE_NAME: &str = "70-librewave-wave3.rules";

/// An exact USB product match with no serial, class, or interface fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactUsbProduct {
    identity: DeviceIdentity,
}

impl ExactUsbProduct {
    /// Returns the admitted Wave:3 normal-mode product.
    #[must_use]
    pub const fn wave3() -> Self {
        Self { identity: DeviceIdentity::wave3() }
    }

    /// Returns the portable admitted identity.
    #[must_use]
    pub const fn identity(self) -> DeviceIdentity {
        self.identity
    }

    /// Tests an observed vendor and product pair for exact equality.
    #[must_use]
    pub const fn matches(self, observed: UsbIdentity) -> bool {
        let expected = self.identity.usb();
        expected.vendor_id == observed.vendor_id && expected.product_id == observed.product_id
    }
}

/// The destination class for one setup-managed policy file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyInstallScope {
    /// A `WirePlumber` 0.5 fragment in the user's configuration.
    UserWirePlumberFragment,
    /// A system udev rules file.
    SystemUdevRules,
}

/// The two policy artifacts required for physical PCM ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyArtifactKind {
    /// The declarative `WirePlumber` card-disable rule.
    WirePlumber,
    /// The normal-mode USB access rule.
    Udev,
}

/// Setup metadata for one rendered policy artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicyArtifactSpec {
    /// The artifact format and purpose.
    pub kind: PolicyArtifactKind,
    /// The installation scope.
    pub scope: PolicyInstallScope,
    /// The deterministic file name.
    pub file_name: &'static str,
}

/// A prerequisite that setup or the future audio adapter must verify.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnershipPrerequisite {
    /// `WirePlumber` must use the reviewed configuration series.
    WirePlumberSeries { major: u16, minor: u16 },
    /// `PipeWire` must be available for deliberate product endpoints.
    PipeWireAvailable,
    /// The kernel audio driver must remain bound to the device.
    SndUsbAudioBound,
    /// The named policy artifact must be installed and active.
    PolicyActive(PolicyArtifactKind),
    /// The admitted normal-mode device must grant the active user access.
    NormalModeDeviceAccessible,
    /// `WirePlumber` must not create or reserve the physical ALSA card.
    PhysicalAlsaCardUnmanaged,
}

/// The owner of the physical ALSA PCMs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalPcmOwner {
    /// The user-session `librewaved` process.
    LibreWaveDaemon,
}

/// How the daemon reaches the physical audio streams.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalPcmAccess {
    /// Direct ALSA PCM access through `snd_usb_audio`.
    DirectAlsa,
}

/// One ordered operation in the physical audio ownership plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnershipStep {
    /// Verify the exact admitted normal-mode device and ALSA card.
    AdmitExactProduct,
    /// Verify that `WirePlumber` did not create or reserve the physical card.
    VerifyPhysicalCardUnmanaged,
    /// Open the physical capture PCM before playback.
    OpenCapturePcm,
    /// Consume frames from the capture PCM inside the daemon.
    ConsumeCaptureFrames,
    /// Confirm that capture has produced at least one frame.
    ConfirmCaptureFrames,
    /// Open the physical playback PCM after capture confirmation.
    OpenPlaybackPcm,
    /// Publish only the endpoint contracts defined by `librewave-core`.
    PublishCoreEndpoints,
    /// Restore saved routes and software levels.
    RestoreSoftwareRouting,
}

const POLICY_ARTIFACTS: [PolicyArtifactSpec; 2] = [
    PolicyArtifactSpec {
        kind: PolicyArtifactKind::WirePlumber,
        scope: PolicyInstallScope::UserWirePlumberFragment,
        file_name: WIREPLUMBER_POLICY_FILE_NAME,
    },
    PolicyArtifactSpec {
        kind: PolicyArtifactKind::Udev,
        scope: PolicyInstallScope::SystemUdevRules,
        file_name: UDEV_POLICY_FILE_NAME,
    },
];

const OWNERSHIP_PREREQUISITES: [OwnershipPrerequisite; 7] = [
    OwnershipPrerequisite::WirePlumberSeries { major: 0, minor: 5 },
    OwnershipPrerequisite::PipeWireAvailable,
    OwnershipPrerequisite::SndUsbAudioBound,
    OwnershipPrerequisite::PolicyActive(PolicyArtifactKind::WirePlumber),
    OwnershipPrerequisite::PolicyActive(PolicyArtifactKind::Udev),
    OwnershipPrerequisite::NormalModeDeviceAccessible,
    OwnershipPrerequisite::PhysicalAlsaCardUnmanaged,
];

const OWNERSHIP_STEPS: [OwnershipStep; 8] = [
    OwnershipStep::AdmitExactProduct,
    OwnershipStep::VerifyPhysicalCardUnmanaged,
    OwnershipStep::OpenCapturePcm,
    OwnershipStep::ConsumeCaptureFrames,
    OwnershipStep::ConfirmCaptureFrames,
    OwnershipStep::OpenPlaybackPcm,
    OwnershipStep::PublishCoreEndpoints,
    OwnershipStep::RestoreSoftwareRouting,
];

/// The static direct-ALSA ownership plan for the admitted Wave:3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioOwnershipPlan {
    target: ExactUsbProduct,
}

impl AudioOwnershipPlan {
    /// Returns the exact product governed by this plan.
    #[must_use]
    pub const fn target(self) -> ExactUsbProduct {
        self.target
    }

    /// Returns the sole physical PCM owner.
    #[must_use]
    pub const fn physical_owner(self) -> PhysicalPcmOwner {
        PhysicalPcmOwner::LibreWaveDaemon
    }

    /// Returns the required physical PCM access method.
    #[must_use]
    pub const fn physical_access(self) -> PhysicalPcmAccess {
        PhysicalPcmAccess::DirectAlsa
    }

    /// Returns the prerequisites in deterministic verification order.
    #[must_use]
    pub const fn prerequisites(self) -> &'static [OwnershipPrerequisite] {
        &OWNERSHIP_PREREQUISITES
    }

    /// Returns the one normal startup sequence.
    #[must_use]
    pub const fn steps(self) -> &'static [OwnershipStep] {
        &OWNERSHIP_STEPS
    }

    /// Returns the user-facing endpoints from the portable core contract.
    #[must_use]
    pub const fn endpoints(self) -> &'static [EndpointId] {
        DELIBERATE_ENDPOINTS
    }
}

/// The complete render-only Linux policy for the admitted Wave:3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wave3AudioPolicy {
    target: ExactUsbProduct,
}

impl Wave3AudioPolicy {
    /// Creates the reviewed policy for the Wave:3 normal-mode identity.
    #[must_use]
    pub const fn new() -> Self {
        Self { target: ExactUsbProduct::wave3() }
    }

    /// Returns setup metadata for the two required artifacts.
    #[must_use]
    pub const fn artifacts(self) -> &'static [PolicyArtifactSpec] {
        &POLICY_ARTIFACTS
    }

    /// Returns the physical ownership plan tied to this policy.
    #[must_use]
    pub const fn ownership_plan(self) -> AudioOwnershipPlan {
        AudioOwnershipPlan { target: self.target }
    }

    /// Renders the minimal `WirePlumber` 0.5 device-disable fragment.
    #[must_use]
    pub fn render_wireplumber(self) -> String {
        let usb = self.target.identity().usb();
        format!(
            concat!(
                "monitor.alsa.rules = [\n",
                "  {{\n",
                "    matches = [\n",
                "      {{\n",
                "        device.vendor.id = \"{}\"\n",
                "        device.product.id = \"{}\"\n",
                "      }}\n",
                "    ]\n",
                "    actions = {{\n",
                "      update-props = {{\n",
                "        device.disabled = true\n",
                "      }}\n",
                "    }}\n",
                "  }}\n",
                "]\n"
            ),
            format_args!("{:#06x}", usb.vendor_id),
            format_args!("{:#06x}", usb.product_id)
        )
    }

    /// Renders the exact normal-mode USB access rule.
    #[must_use]
    pub fn render_udev(self) -> String {
        let usb = self.target.identity().usb();
        format!(
            "SUBSYSTEM==\"usb\", ENV{{DEVTYPE}}==\"usb_device\", ATTR{{idVendor}}==\"{:04x}\", ATTR{{idProduct}}==\"{:04x}\", TAG+=\"uaccess\"\n",
            usb.vendor_id, usb.product_id
        )
    }
}

impl Default for Wave3AudioPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIREPLUMBER_GOLDEN: &str = include_str!("../tests/golden/80-librewave-wave3.conf");
    const UDEV_GOLDEN: &str = include_str!("../tests/golden/70-librewave-wave3.rules");

    #[test]
    fn renderers_match_reviewed_golden_files() {
        let policy = Wave3AudioPolicy::new();
        assert_eq!(policy.render_wireplumber(), WIREPLUMBER_GOLDEN);
        assert_eq!(policy.render_udev(), UDEV_GOLDEN);
    }

    #[test]
    fn exact_product_rejects_nearby_vendor_and_product_ids() {
        let target = ExactUsbProduct::wave3();
        assert!(target.matches(UsbIdentity::new(0x0fd9, 0x0070)));
        assert!(!target.matches(UsbIdentity::new(0x0fd8, 0x0070)));
        assert!(!target.matches(UsbIdentity::new(0x0fda, 0x0070)));
        assert!(!target.matches(UsbIdentity::new(0x0fd9, 0x006f)));
        assert!(!target.matches(UsbIdentity::new(0x0fd9, 0x0071)));
    }

    #[test]
    fn rendered_policy_has_no_broad_or_serial_match() {
        let policy = Wave3AudioPolicy::new();
        let wireplumber = policy.render_wireplumber();
        let udev = policy.render_udev();

        assert!(!wireplumber.contains('~'));
        assert!(!wireplumber.contains("node."));
        assert!(!wireplumber.contains("serial"));
        assert!(!wireplumber.contains("script"));
        assert!(!wireplumber.contains("lua"));
        assert!(!wireplumber.contains("wireplumber.components"));
        assert!(!udev.contains("MODE="));
        assert!(!udev.contains("bDeviceClass"));
        assert!(!udev.contains("bInterface"));
        assert!(!udev.contains("serial"));
    }

    #[test]
    fn ownership_plan_is_capture_first_and_uses_core_endpoints() {
        let plan = Wave3AudioPolicy::new().ownership_plan();
        assert_eq!(plan.physical_owner(), PhysicalPcmOwner::LibreWaveDaemon);
        assert_eq!(plan.physical_access(), PhysicalPcmAccess::DirectAlsa);
        assert_eq!(
            plan.prerequisites(),
            &[
                OwnershipPrerequisite::WirePlumberSeries { major: 0, minor: 5 },
                OwnershipPrerequisite::PipeWireAvailable,
                OwnershipPrerequisite::SndUsbAudioBound,
                OwnershipPrerequisite::PolicyActive(PolicyArtifactKind::WirePlumber),
                OwnershipPrerequisite::PolicyActive(PolicyArtifactKind::Udev),
                OwnershipPrerequisite::NormalModeDeviceAccessible,
                OwnershipPrerequisite::PhysicalAlsaCardUnmanaged,
            ]
        );
        assert_eq!(
            plan.steps(),
            &[
                OwnershipStep::AdmitExactProduct,
                OwnershipStep::VerifyPhysicalCardUnmanaged,
                OwnershipStep::OpenCapturePcm,
                OwnershipStep::ConsumeCaptureFrames,
                OwnershipStep::ConfirmCaptureFrames,
                OwnershipStep::OpenPlaybackPcm,
                OwnershipStep::PublishCoreEndpoints,
                OwnershipStep::RestoreSoftwareRouting,
            ]
        );
        assert_eq!(plan.endpoints(), DELIBERATE_ENDPOINTS);
    }

    #[test]
    fn setup_artifacts_are_stable_and_scoped() {
        assert_eq!(
            Wave3AudioPolicy::new().artifacts(),
            &[
                PolicyArtifactSpec {
                    kind: PolicyArtifactKind::WirePlumber,
                    scope: PolicyInstallScope::UserWirePlumberFragment,
                    file_name: "80-librewave-wave3.conf",
                },
                PolicyArtifactSpec {
                    kind: PolicyArtifactKind::Udev,
                    scope: PolicyInstallScope::SystemUdevRules,
                    file_name: "70-librewave-wave3.rules",
                },
            ]
        );
    }
}
