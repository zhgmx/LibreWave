//! Control-side mapping from stable product identities to fixed DSP slots.
use librewave_core::{
    FaderGain, InputName, MixRoute, MixerProfile, SourceControls, SourceId, SourceRole,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InputLayout {
    slots: [Option<SourceId>; 10],
}

impl InputLayout {
    pub(super) fn new(profile: &MixerProfile) -> Self {
        Self { slots: [None; 10] }.updated(profile)
    }

    pub(super) fn updated(&self, profile: &MixerProfile) -> Self {
        let mut next = self.clone();
        for slot in &mut next.slots {
            if slot.is_some_and(|id| profile.source(id).is_none()) {
                *slot = None;
            }
        }
        for input in profile.sources.iter().filter(|s| s.role == SourceRole::Software) {
            let id = input.controls.source();
            if !next.slots.contains(&Some(id))
                && let Some(slot) = next.slots.iter_mut().find(|s| s.is_none())
            {
                *slot = Some(id);
            }
        }
        next
    }

    pub(super) fn slot(&self, source: SourceId) -> Option<SourceId> {
        if source.get() <= 2 {
            return Some(source);
        }
        self.slots
            .iter()
            .position(|s| *s == Some(source))
            .map(|index| SourceId::new(u16::try_from(index).expect("bounded slot") + 3))
    }

    pub(super) fn active_mask(&self) -> u32 {
        self.slots
            .iter()
            .enumerate()
            .fold(1, |mask, (i, source)| mask | if source.is_some() { 1 << (i + 1) } else { 0 })
    }

    pub(super) fn pairs(&self) -> Vec<(SourceId, SourceId)> {
        [(SourceId::new(1), SourceId::new(1)), (SourceId::new(2), SourceId::new(2))]
            .into_iter()
            .chain(self.slots.iter().enumerate().filter_map(|(i, source)| {
                source.map(|source| {
                    (source, SourceId::new(u16::try_from(i).expect("bounded slot") + 3))
                })
            }))
            .collect()
    }

    pub(super) fn profile(&self, profile: &MixerProfile) -> MixerProfile {
        let mut dsp = MixerProfile::default();
        for _ in 0..10 {
            dsp = dsp
                .with_input(InputName::try_new("Reserved").expect("constant name"))
                .expect("fixed slot capacity");
        }
        let muted = MixRoute::new(false, FaderGain::UNITY);
        let pairs = self.pairs();
        for input in &mut dsp.sources {
            let slot = input.controls.source();
            input.controls = pairs
                .iter()
                .find(|(_, candidate)| *candidate == slot)
                .and_then(|(source, _)| profile.source(*source))
                .map_or(SourceControls::new(slot, muted, muted), |source| {
                    SourceControls::new(slot, source.controls.monitor(), source.controls.stream())
                });
        }
        dsp.outputs = profile.outputs;
        dsp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_preserve_unaffected_slots_and_mute_vacant_slots() {
        let original = MixerProfile::first_run(&[]);
        let layout = InputLayout::new(&original);
        let removed = original.without_input(SourceId::new(3)).expect("remove");
        let next = removed.with_input(InputName::try_new("Extra").expect("name")).expect("add");
        let updated = layout.updated(&next);
        assert_eq!(updated.slot(SourceId::new(4)), layout.slot(SourceId::new(4)));
        assert_eq!(updated.slot(SourceId::new(7)), Some(SourceId::new(3)));
        let dsp = layout.updated(&removed).profile(&removed);
        assert_eq!(dsp.sources.len(), 12);
        assert!(!dsp.source(SourceId::new(3)).expect("reserved").controls.monitor().enabled());
        dsp.validate().expect("valid DSP profile");
    }
}
