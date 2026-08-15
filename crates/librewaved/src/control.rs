use librewave_core::{FixedPointValue, VolumeSelection, Wave3ConfigSnapshot, Wave3Control};
use librewave_device::{TransactionError, TransactionOutcome, TransportError, Wave3WriteState};
use librewave_platform_linux::{Wave3UsbConnection, wave3_config_snapshot};
use librewave_protocol::{
    ApiVersion, VolumeSelect, Wave3ControlChange, Wave3GainDb, Wave3HeadphoneDb,
    Wave3MonitorPercent,
};

pub(crate) trait ManagedWave3Connection {
    fn api(&self) -> ApiVersion;
    fn observed(&self) -> Result<Wave3ConfigSnapshot, String>;
    fn apply(&mut self, control: Wave3Control) -> ConnectionOutcome;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionOutcome {
    Applied(Wave3ConfigSnapshot),
    Unchanged(Wave3ConfigSnapshot),
    StaleBaseline(Wave3ConfigSnapshot),
    Invalid(String),
    Disconnected,
    Failed { observed: Option<Wave3ConfigSnapshot>, restoration_unverified: bool, message: String },
}

impl ManagedWave3Connection for Wave3UsbConnection {
    fn api(&self) -> ApiVersion {
        self.api()
    }

    fn observed(&self) -> Result<Wave3ConfigSnapshot, String> {
        wave3_config_snapshot(self.config()).map_err(|error| error.to_string())
    }

    fn apply(&mut self, control: Wave3Control) -> ConnectionOutcome {
        let change = match protocol_change(control) {
            Ok(change) => change,
            Err(message) => return ConnectionOutcome::Invalid(message),
        };
        let expected = *self.config();
        match self.transact_control(expected, change) {
            TransactionOutcome::Applied { config } => snapshot_outcome(config, true),
            TransactionOutcome::Unchanged { config } => snapshot_outcome(config, false),
            TransactionOutcome::Rejected {
                error: TransactionError::StaleBaseline { actual, .. },
            } => match wave3_config_snapshot(&actual) {
                Ok(config) => ConnectionOutcome::StaleBaseline(config),
                Err(_) => ConnectionOutcome::Failed {
                    observed: self.observed().ok(),
                    restoration_unverified: true,
                    message: "stale Wave:3 baseline could not be decoded".to_owned(),
                },
            },
            TransactionOutcome::Rejected { error: TransactionError::InvalidChange(error) } => {
                ConnectionOutcome::Invalid(error.to_string())
            }
            TransactionOutcome::Rejected { error } if disconnected(&error) => {
                ConnectionOutcome::Disconnected
            }
            TransactionOutcome::Rejected { error } => ConnectionOutcome::Failed {
                observed: self.observed().ok(),
                restoration_unverified: self.write_state() == Wave3WriteState::NeedsReprobe,
                message: error.to_string(),
            },
            TransactionOutcome::Failed { primary, restoration } => {
                if disconnected(&primary)
                    && matches!(
                        restoration.verification,
                        Err(TransactionError::Transport {
                            error: TransportError::Disconnected | TransportError::NotFound,
                            ..
                        })
                    )
                {
                    return ConnectionOutcome::Disconnected;
                }
                let restoration_unverified = restoration.verification.is_err();
                let observed = restoration
                    .verification
                    .ok()
                    .and_then(|config| wave3_config_snapshot(&config).ok())
                    .or_else(|| self.observed().ok());
                ConnectionOutcome::Failed {
                    observed,
                    restoration_unverified,
                    message: primary.to_string(),
                }
            }
        }
    }
}

fn snapshot_outcome(config: librewave_protocol::Wave3Config, applied: bool) -> ConnectionOutcome {
    match wave3_config_snapshot(&config) {
        Ok(config) if applied => ConnectionOutcome::Applied(config),
        Ok(config) => ConnectionOutcome::Unchanged(config),
        Err(_) => ConnectionOutcome::Failed {
            observed: None,
            restoration_unverified: true,
            message: "transaction readback could not be decoded".to_owned(),
        },
    }
}

fn protocol_change(control: Wave3Control) -> Result<Wave3ControlChange, String> {
    match control {
        Wave3Control::InputGain(value) => {
            fixed(value, Wave3GainDb::from_raw_q8_8).map(Wave3ControlChange::MicrophoneGain)
        }
        Wave3Control::MicrophoneMute(value) => Ok(Wave3ControlChange::MicrophoneMute(value)),
        Wave3Control::Clipguard(value) => Ok(Wave3ControlChange::Clipguard(value)),
        Wave3Control::LowCut(value) => Ok(Wave3ControlChange::Lowcut(value)),
        Wave3Control::HeadphoneLevel(value) => {
            fixed(value, Wave3HeadphoneDb::from_raw_q8_8).map(Wave3ControlChange::HeadphoneVolume)
        }
        Wave3Control::HeadphoneMute(value) => Ok(Wave3ControlChange::HeadphoneMute(value)),
        Wave3Control::MonitorMix(value) => {
            fixed(value, Wave3MonitorPercent::from_raw_q8_8).map(Wave3ControlChange::DirectMonitor)
        }
        Wave3Control::KnobTarget(value) => Ok(Wave3ControlChange::VolumeSelect(match value {
            VolumeSelection::Microphone => VolumeSelect::Mic,
            VolumeSelection::Headphone => VolumeSelect::Headphone,
            VolumeSelection::Mix => VolumeSelect::Mix,
        })),
        Wave3Control::AllLedsOff(value) => Ok(Wave3ControlChange::AllLedsOff(value)),
        Wave3Control::LedsFlip(value) => Ok(Wave3ControlChange::LedsFlip(value)),
        Wave3Control::GainLock(value) => Ok(Wave3ControlChange::GainLock(value)),
    }
}

pub(crate) fn validate_control(control: Wave3Control) -> Result<(), String> {
    protocol_change(control).map(|_| ())
}

fn fixed<T>(
    value: FixedPointValue,
    construct: impl FnOnce(i32) -> Result<T, librewave_protocol::ValueError>,
) -> Result<T, String> {
    if value.fractional_bits != 8 {
        return Err(format!(
            "fixed-point value uses {} fractional bits; expected 8",
            value.fractional_bits
        ));
    }
    construct(value.raw).map_err(|error| error.to_string())
}

fn disconnected(error: &TransactionError) -> bool {
    matches!(
        error,
        TransactionError::Transport {
            error: TransportError::Disconnected | TransportError::NotFound,
            ..
        }
    )
}
