//! USB observation on the serialized daemon control loop. Reads never write,
//! except the saved-level restore on a new USB connection.
use super::{
    AdmissionPurpose, Daemon, DeviceAdmissionSnapshot, DeviceId, DeviceWriteLockReason, Duration,
    Instant, ignore_reconciliation_event,
};
use librewave_core::{AdmissionError, Origin};

const READ_INTERVAL: Duration = Duration::from_millis(500);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const INVENTORY_INTERVAL: Duration = Duration::from_secs(1);

impl Daemon {
    pub(crate) fn control_tick(&mut self) {
        self.audio_tick();
        let now = Instant::now();
        if now >= self.next_control_inventory {
            self.next_control_inventory = now + INVENTORY_INTERVAL;
            ignore_reconciliation_event(self.refresh_inventory(Origin::Device));
        }
        self.observe_controls(now);
    }

    pub(super) fn observe_controls(&mut self, now: Instant) {
        self.control_reads.retain(|id, _| self.candidates.contains_key(id));
        // At most one device read per loop turn. Earliest deadlines and device
        // IDs make multiple devices fair without a burst of USB transfers.
        let due: Option<DeviceId> = self
            .candidates
            .keys()
            .filter(|id| self.can_observe(**id))
            .map(|id| (*id, self.control_reads.get(id).copied().unwrap_or(now)))
            .filter(|(_, deadline)| *deadline <= now)
            .min_by_key(|(id, deadline)| (*deadline, *id))
            .map(|(id, _)| id);
        let Some(id) = due else { return };
        if let Err(error) = self.admit(id, Origin::Device, AdmissionPurpose::Observe) {
            eprintln!("librewaved: device observation failed: {error}");
        }
        // Schedule from completion, so a slow read cannot cause catch-up reads.
        let interval =
            if self.connections.contains_key(&id) { READ_INTERVAL } else { RETRY_INTERVAL };
        self.control_reads.insert(id, Instant::now().max(now) + interval);
    }

    fn can_observe(&self, id: DeviceId) -> bool {
        if let Some(owned) = self.connections.get(&id) {
            return matches!(
                owned.locked,
                None | Some(DeviceWriteLockReason::PersistenceAmbiguous)
            );
        }
        self.device(id).is_ok_and(|device| {
            matches!(
                device.admission,
                DeviceAdmissionSnapshot::NotInspected
                    | DeviceAdmissionSnapshot::Failed {
                        error: AdmissionError::PermissionDenied | AdmissionError::Disconnected
                    }
            )
        })
    }
}
