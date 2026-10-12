//! Control-side stream projection and conditional metadata transactions.
use librewave_core::{
    Application, ApplicationId, ApplicationMovement, ApplicationRoute, InputName, SourceId,
};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

pub(super) const MAX_STREAMS: usize = 128;
const MOVE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(super) struct Stream {
    pub node: u32,
    pub serial: u64,
    pub identity: ApplicationId,
    pub name: InputName,
    pub blocked: bool,
    pub ports: Vec<u32>,
    pub links: Vec<(u32, u32)>,
}

impl Stream {
    fn linked_to(&self, target: u32) -> bool {
        !self.ports.is_empty()
            && self.ports.iter().all(|port| self.links.contains(&(*port, target)))
            && self.links.iter().all(|(_, node)| *node == target)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MetadataValue {
    pub type_: Option<String>,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Write {
    pub subject: u32,
    pub value: Option<MetadataValue>,
}

struct Member {
    node: u32,
    serial: u64,
    previous: Option<MetadataValue>,
    acknowledged: bool,
}
struct Pending {
    application: ApplicationId,
    source: SourceId,
    target: u32,
    target_serial: u64,
    desired: MetadataValue,
    members: Vec<Member>,
    deadline: Instant,
    overridden: bool,
}
struct Rollback {
    application: ApplicationId,
    reason: String,
    desired: MetadataValue,
    members: Vec<Member>,
    deadline: Instant,
}

#[derive(Default)]
pub(super) struct Router {
    pub revision: u64,
    pub targets: HashMap<u32, MetadataValue>,
    pub moving: Option<bool>,
    pub default_ready: bool,
    pub settings_ready: bool,
    pending: Option<Pending>,
    rollback: Option<Rollback>,
    pub result: Option<ApplicationRoute>,
}

impl Router {
    pub fn update_target(&mut self, subject: u32, value: Option<MetadataValue>) {
        if let Some(pending) = &mut self.pending {
            for member in
                pending.members.iter_mut().filter(|m| m.node == subject || subject == u32::MAX)
            {
                if value.as_ref() == Some(&pending.desired) {
                    member.acknowledged = true;
                } else if member.acknowledged {
                    pending.overridden = true;
                }
            }
        }
        if subject == u32::MAX {
            self.targets.clear();
        } else if let Some(value) = value {
            self.targets.insert(subject, value);
        } else {
            self.targets.remove(&subject);
        }
        self.changed();
    }
    pub fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    pub fn movement(&self) -> ApplicationMovement {
        if !self.default_ready || !self.settings_ready {
            return ApplicationMovement::Unavailable;
        }
        match self.moving {
            Some(true) => ApplicationMovement::Allowed,
            Some(false) => ApplicationMovement::Disabled,
            None => ApplicationMovement::Unavailable,
        }
    }

    pub fn project(streams: &[Stream], sinks: &[(SourceId, u32, u64)]) -> Vec<Application> {
        let mut groups = BTreeMap::<ApplicationId, Vec<&Stream>>::new();
        for stream in streams {
            groups.entry(stream.identity).or_default().push(stream);
        }
        groups
            .into_iter()
            .map(|(id, group)| Application {
                id,
                name: group.iter().map(|s| s.name).min().expect("nonempty group"),
                streams: group.len(),
                move_blocked: group.iter().any(|s| s.blocked),
                source: sinks
                    .iter()
                    .find(|(_, node, _)| group.iter().all(|s| s.linked_to(*node)))
                    .map(|s| s.0),
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin(
        &mut self,
        revision: u64,
        application: ApplicationId,
        source: SourceId,
        target_name: &str,
        streams: &[Stream],
        sinks: &[(SourceId, u32, u64)],
        now: Instant,
    ) -> Result<Vec<Write>, String> {
        if revision != self.revision {
            return Err("Application observations changed; refresh the application list.".into());
        }
        if self.pending.is_some() || self.rollback.is_some() {
            return Err("Another application move is pending.".into());
        }
        if self.movement() != ApplicationMovement::Allowed {
            return Err("WirePlumber stream movement is disabled or unavailable.".into());
        }
        let (_, target, target_serial) = sinks
            .iter()
            .find(|s| s.0 == source)
            .ok_or_else(|| "The selected software input is absent.".to_owned())?;
        let group: Vec<_> = streams.iter().filter(|s| s.identity == application).collect();
        if group.is_empty() || group.len() > MAX_STREAMS {
            return Err("The application has no bounded active stream group.".into());
        }
        if group.iter().any(|s| s.blocked || s.serial == 0) {
            return Err(
                "An application stream forbids movement or lacks a verified incarnation.".into()
            );
        }
        let desired = MetadataValue { type_: Some("Spa:Id".into()), value: target_name.to_owned() };
        let members = group
            .iter()
            .map(|s| Member {
                node: s.node,
                serial: s.serial,
                previous: self.targets.get(&s.node).cloned(),
                acknowledged: false,
            })
            .collect();
        let writes =
            group.iter().map(|s| Write { subject: s.node, value: Some(desired.clone()) }).collect();
        self.pending = Some(Pending {
            application,
            source,
            target: *target,
            target_serial: *target_serial,
            desired,
            members,
            deadline: now + MOVE_TIMEOUT,
            overridden: false,
        });
        self.result = Some(ApplicationRoute::Pending { application, source });
        Ok(writes)
    }

    pub fn advance(
        &mut self,
        streams: &[Stream],
        sinks: &[(SourceId, u32, u64)],
        now: Instant,
    ) -> Vec<Write> {
        if let Some(rollback) = &self.rollback {
            let resolved = self.default_ready
                && rollback.members.iter().all(|m| {
                    !streams.iter().any(|s| s.node == m.node && s.serial == m.serial)
                        || self.targets.get(&m.node) == m.previous.as_ref()
                        || self.targets.get(&m.node) != Some(&rollback.desired)
                });
            if resolved || !self.default_ready || now >= rollback.deadline {
                let suffix = if resolved {
                    "Prior metadata acknowledged or external changes preserved."
                } else {
                    "Metadata restoration could not be confirmed."
                };
                self.result = Some(ApplicationRoute::Failed {
                    application: rollback.application,
                    reason: format!("{} {suffix}", rollback.reason),
                });
                self.rollback = None;
            }
            return Vec::new();
        }
        let Some(pending) = &self.pending else {
            return Vec::new();
        };
        let group: Vec<_> = streams.iter().filter(|s| s.identity == pending.application).collect();
        let failure = if self.movement() != ApplicationMovement::Allowed {
            Some("WirePlumber movement policy disappeared or changed.")
        } else if !sinks.contains(&(pending.source, pending.target, pending.target_serial)) {
            Some("The selected input disappeared or was replaced.")
        } else if group.len() != pending.members.len()
            || pending
                .members
                .iter()
                .any(|m| !group.iter().any(|s| s.node == m.node && s.serial == m.serial))
        {
            Some("The application stream group changed during movement.")
        } else if group.iter().any(|s| s.blocked) {
            Some("An application stream now forbids movement.")
        } else if pending.overridden
            || pending.members.iter().any(|m| {
                self.targets
                    .get(&m.node)
                    .is_some_and(|v| v != &pending.desired && Some(v) != m.previous.as_ref())
            })
        {
            Some("The desktop changed an application target during movement.")
        } else if now >= pending.deadline {
            Some("WirePlumber did not verify every application audio link within two seconds.")
        } else {
            None
        };
        if let Some(reason) = failure {
            return self.cancel(streams, reason);
        }
        if pending.members.iter().all(|m| self.targets.get(&m.node) == Some(&pending.desired))
            && group.iter().all(|s| s.linked_to(pending.target))
        {
            self.result = Some(ApplicationRoute::Verified {
                application: pending.application,
                source: pending.source,
            });
            self.pending = None;
        }
        Vec::new()
    }

    pub fn cancel(&mut self, streams: &[Stream], reason: &str) -> Vec<Write> {
        let Some(pending) = self.pending.take() else {
            return Vec::new();
        };
        let members: Vec<_> = pending
            .members
            .into_iter()
            .filter(|m| {
                self.default_ready
                    && self.targets.get(&m.node) == Some(&pending.desired)
                    && streams.iter().any(|s| s.node == m.node && s.serial == m.serial)
            })
            .collect();
        let writes: Vec<_> =
            members.iter().map(|m| Write { subject: m.node, value: m.previous.clone() }).collect();
        if writes.is_empty() {
            self.result = Some(ApplicationRoute::Failed {
                application: pending.application,
                reason: reason.into(),
            });
        } else {
            self.result = Some(ApplicationRoute::Restoring {
                application: pending.application,
                reason: reason.into(),
            });
            self.rollback = Some(Rollback {
                application: pending.application,
                reason: reason.into(),
                desired: pending.desired,
                members,
                deadline: Instant::now() + MOVE_TIMEOUT,
            });
        }
        writes
    }
}

pub(super) fn identity(
    properties: &pw::spa::utils::dict::DictRef,
) -> Option<(ApplicationId, InputName)> {
    let id = properties
        .get("application.id")
        .and_then(|s| InputName::try_new(s).ok())
        .map(ApplicationId::Desktop)
        .or_else(|| {
            properties
                .get("application.process.binary")
                .and_then(|s| InputName::try_new(s).ok())
                .map(ApplicationId::Executable)
        })?;
    let fallback = match id {
        ApplicationId::Desktop(v) | ApplicationId::Executable(v) => v,
    };
    Some((
        id,
        properties
            .get("application.name")
            .and_then(|s| InputName::try_new(s).ok())
            .unwrap_or(fallback),
    ))
}

use pipewire as pw;

#[cfg(test)]
mod tests {
    use super::*;
    fn stream(node: u32) -> Stream {
        let name = InputName::try_new("Player").expect("valid fixture");
        Stream {
            node,
            serial: u64::from(node),
            identity: ApplicationId::Executable(name),
            name,
            blocked: false,
            ports: vec![node * 2, node * 2 + 1],
            links: Vec::new(),
        }
    }
    fn router() -> Router {
        Router {
            default_ready: true,
            settings_ready: true,
            moving: Some(true),
            ..Router::default()
        }
    }
    fn sinks() -> [(SourceId, u32, u64); 1] {
        [(SourceId::new(3), 42, 100)]
    }
    #[test]
    fn grouped_move_requires_metadata_ack_and_every_audio_link() {
        let mut r = router();
        let mut streams = vec![stream(1), stream(2)];
        let now = Instant::now();
        assert_eq!(Router::project(&streams, &sinks())[0].streams, 2);
        let writes = r
            .begin(
                0,
                streams[0].identity,
                SourceId::new(3),
                "librewave.input.3",
                &streams,
                &sinks(),
                now,
            )
            .expect("valid fixture");
        assert_eq!(writes.len(), 2);
        for w in writes {
            r.targets.insert(w.subject, w.value.expect("valid fixture"));
        }
        streams[0].links = vec![(2, 42), (3, 42)];
        streams[1].links = vec![(4, 42)];
        assert!(r.advance(&streams, &sinks(), now).is_empty());
        assert!(matches!(r.result, Some(ApplicationRoute::Pending { .. })));
        streams[1].links.push((5, 42));
        r.advance(&streams, &sinks(), now);
        assert!(matches!(r.result, Some(ApplicationRoute::Verified { .. })));
        assert_eq!(Router::project(&streams, &sinks())[0].source, Some(SourceId::new(3)));
    }
    #[test]
    fn preflight_refuses_stale_disabled_protected_and_missing_targets_without_writes() {
        let mut r = router();
        let mut s = vec![stream(1)];
        let now = Instant::now();
        assert!(r.begin(1, s[0].identity, SourceId::new(3), "target", &s, &sinks(), now).is_err());
        r.moving = Some(false);
        assert!(r.begin(0, s[0].identity, SourceId::new(3), "target", &s, &sinks(), now).is_err());
        r.moving = Some(true);
        s[0].blocked = true;
        assert!(r.begin(0, s[0].identity, SourceId::new(3), "target", &s, &sinks(), now).is_err());
        s[0].blocked = false;
        assert!(r.begin(0, s[0].identity, SourceId::new(1), "target", &s, &sinks(), now).is_err());
        assert!(r.pending.is_none());
        assert!(r.targets.is_empty());
    }
    #[test]
    fn timeout_restores_exact_owned_values_but_preserves_desktop_override_and_reused_ids() {
        let mut r = router();
        let mut s = vec![stream(1), stream(2), stream(3)];
        let now = Instant::now();
        let old = MetadataValue { type_: Some("custom".into()), value: "old".into() };
        r.targets.insert(1, old.clone());
        let writes = r
            .begin(0, s[0].identity, SourceId::new(3), "target", &s, &sinks(), now)
            .expect("valid fixture");
        for w in writes {
            r.targets.insert(w.subject, w.value.expect("valid fixture"));
        }
        r.targets.insert(2, MetadataValue { type_: None, value: "desktop".into() });
        s[2].serial += 1;
        let rollback = r.advance(&s, &sinks(), now + MOVE_TIMEOUT);
        assert_eq!(rollback, vec![Write { subject: 1, value: Some(old) }]);
        assert!(matches!(r.result, Some(ApplicationRoute::Restoring { .. })));
    }
    #[test]
    fn restart_and_new_stream_cancel_without_replaying_assignments() {
        let mut r = router();
        let mut s = vec![stream(1)];
        let now = Instant::now();
        let w = r
            .begin(0, s[0].identity, SourceId::new(3), "target", &s, &sinks(), now)
            .expect("valid fixture");
        r.targets.insert(1, w[0].value.clone().expect("valid fixture"));
        s.push(stream(2));
        assert_eq!(r.advance(&s, &sinks(), now), vec![Write { subject: 1, value: None }]);
        assert!(r.pending.is_none());
        r.default_ready = false;
        r.targets.clear();
        assert_eq!(r.movement(), ApplicationMovement::Unavailable);
    }

    #[test]
    fn desktop_clear_after_acknowledgement_cancels_without_overwriting_it() {
        let mut r = router();
        let streams = vec![stream(1)];
        let now = Instant::now();
        let writes = r
            .begin(0, streams[0].identity, SourceId::new(3), "target", &streams, &sinks(), now)
            .expect("move");
        r.update_target(1, writes[0].value.clone());
        r.update_target(1, None);
        assert!(r.advance(&streams, &sinks(), now).is_empty());
        assert!(
            matches!(r.result,Some(ApplicationRoute::Failed { ref reason, .. }) if reason.contains("desktop"))
        );
        assert!(!r.targets.contains_key(&1));
    }

    #[test]
    fn stable_identity_prefers_desktop_id_and_never_uses_process_ids_or_titles() {
        let properties = pipewire::properties::properties! {
            "application.id" => "org.test.Player", "application.name" => "Player",
            "application.process.binary" => "player", "application.process.id" => "1234",
            "media.name" => "A changing tab title"
        };
        let (id, name) = identity(properties.as_ref()).expect("stable identity");
        assert_eq!(id, ApplicationId::Desktop(InputName::try_new("org.test.Player").expect("id")));
        assert_eq!(name.as_str(), "Player");
        let properties = pipewire::properties::properties! { "application.process.id" => "1234", "media.name" => "A changing tab title" };
        assert!(identity(properties.as_ref()).is_none());
    }

    #[test]
    fn rollback_is_pending_until_previous_metadata_is_acknowledged() {
        let mut r = router();
        let streams = vec![stream(1)];
        let now = Instant::now();
        let writes = r
            .begin(0, streams[0].identity, SourceId::new(3), "target", &streams, &sinks(), now)
            .expect("begin");
        r.update_target(1, writes[0].value.clone());
        let rollback = r.advance(&streams, &sinks(), now + MOVE_TIMEOUT);
        assert_eq!(rollback, vec![Write { subject: 1, value: None }]);
        assert!(matches!(r.result, Some(ApplicationRoute::Restoring { .. })));
        assert!(
            r.begin(
                r.revision,
                streams[0].identity,
                SourceId::new(3),
                "target",
                &streams,
                &sinks(),
                now
            )
            .is_err()
        );
        r.update_target(1, None);
        r.advance(&streams, &sinks(), now);
        assert!(
            matches!(r.result,Some(ApplicationRoute::Failed { ref reason, .. }) if reason.contains("acknowledged"))
        );
        assert!(r.rollback.is_none());
    }
}
