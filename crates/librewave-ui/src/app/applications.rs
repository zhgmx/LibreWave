//! Active application selection stays on the versioned daemon interface.
use super::{
    Shell,
    controls::{Tone, action_button, caption, icon_button},
    theme::{FAINT, INK, SPACE_MD},
};
use gpui::{Context, Div, div, prelude::*, px};
use librewave_core::{
    ApplicationId, ApplicationMovement, ApplicationRoute, ApplicationSnapshot, Command, InputName,
    Snapshot, SourceId, SourceRole,
};
use librewave_ipc::Response;
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum RuleKind {
    Desktop,
    Executable,
}

impl Shell {
    pub(super) fn close_channel(&mut self, cx: &mut Context<Self>) {
        self.application_input = None;
        self.channel_name = None;
        self.assignment_name = None;
        cx.notify();
    }

    pub(super) fn open_channel(&mut self, source: SourceId, cx: &mut Context<Self>) {
        self.channel_name = None;
        self.assignment_name = None;
        if self.application_input == Some(source) {
            self.application_input = None;
            return;
        }
        self.application_input = Some(source);
        if let Some(input) = self.snapshot.as_ref().and_then(|s| s.mixer.profile.source(source))
            && input.role == SourceRole::Software
        {
            let name = input.name.as_str().to_owned();
            self.channel_name = Some(cx.new(|cx| super::text_field::TextField::new(&name, cx)));
        }
        self.assignment_name = Some(cx.new(|cx| super::text_field::TextField::new("", cx)));
    }

    fn save_channel_text(&mut self, assignment: bool, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else { return };
        let Some(source) = self.application_input else { return };
        let field = if assignment { &self.assignment_name } else { &self.channel_name };
        let Some(field) = field else { return };
        let name = match InputName::try_new(field.read(cx).value()) {
            Ok(name) => name,
            Err(error) => {
                self.action_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let expected_generation = snapshot.mixer.generation();
        let command = if assignment {
            Command::SetApplicationAssignment {
                expected_generation,
                application: if self.assignment_kind == RuleKind::Desktop {
                    ApplicationId::Desktop(name)
                } else {
                    ApplicationId::Executable(name)
                },
                source: Some(source),
            }
        } else {
            Command::RenameMixerInput { expected_generation, source, name }
        };
        self.send(command, cx);
    }

    fn channel_controls(
        &self,
        snapshot: &Snapshot,
        source: SourceId,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut view = div().flex().flex_col().gap(px(SPACE_MD));
        if let Some(field) = &self.channel_name {
            view = view.child(caption("Channel name")).child(
                div().flex().gap(px(SPACE_MD)).child(field.clone()).child(action_button(
                    "rename-channel",
                    "Save name",
                    Tone::Secondary,
                    !self.pending,
                    cx,
                    |this, _, cx| this.save_channel_text(false, cx),
                )),
            );
        }
        if let Some(position) =
            snapshot.mixer.profile.sources.iter().position(|s| s.controls.source() == source)
        {
            let mut order = div().flex().gap(px(SPACE_MD));
            for (id, label, target) in [
                ("channel-earlier", "Move left", position.checked_sub(1)),
                (
                    "channel-later",
                    "Move right",
                    (position + 1 < snapshot.mixer.profile.sources.len()).then_some(position + 1),
                ),
            ] {
                let generation = snapshot.mixer.generation();
                order = order.child(action_button(
                    id,
                    label,
                    Tone::Quiet,
                    !self.pending && target.is_some(),
                    cx,
                    move |this, _, cx| {
                        if let Some(position) = target {
                            this.send(
                                Command::MoveMixerInput {
                                    expected_generation: generation,
                                    source,
                                    position: u16::try_from(position)
                                        .expect("bounded source count"),
                                },
                                cx,
                            );
                        }
                    },
                ));
            }
            view = view.child(order);
        }
        if let Some(field) = &self.assignment_name {
            view = view.child(caption("Saved application rules apply when an application starts. Enter its exact desktop ID or executable name."))
                .child(div().flex().gap(px(SPACE_MD))
                    .child(action_button("rule-kind", if self.assignment_kind == RuleKind::Desktop { "Desktop ID" } else { "Executable" }, Tone::Secondary, !self.pending, cx,
                        |this, _, cx| { this.assignment_kind = if this.assignment_kind == RuleKind::Desktop { RuleKind::Executable } else { RuleKind::Desktop }; cx.notify(); }))
                    .child(field.clone())
                    .child(action_button("save-rule", "Save rule", Tone::Secondary, !self.pending, cx,
                        |this, _, cx| this.save_channel_text(true, cx))));
        }
        view.child(action_button("close-channel", "Close", Tone::Quiet, true, cx, |this, _, cx| {
            this.close_channel(cx);
        }))
    }

    pub(super) fn poll_applications(&mut self, cx: &mut Context<Self>) {
        if !self.audio_active()
            || self.applications_received.is_some_and(|t| t.elapsed() > Duration::from_millis(1500))
        {
            if self.applications.take().is_some() {
                cx.notify();
            }
            self.applications_received = None;
        }
        if self.pending || !self.audio_active() || self.application_request_epoch.is_some() {
            return;
        }
        let epoch = self.request_epoch;
        self.application_request_epoch = Some(epoch);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    Client::new(configured_socket_path()).request(Command::GetApplications)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.apply_application_result(epoch, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_application_result(
        &mut self,
        epoch: u64,
        result: Result<Response, ClientError>,
    ) -> bool {
        self.application_request_epoch = None;
        if epoch != self.request_epoch || !self.audio_active() {
            return false;
        }
        let (next, error) = match result {
            Ok(Response::Applications { applications }) => (Some(applications), None),
            Ok(_) => {
                (None, Some("The daemon returned an unexpected application response.".to_owned()))
            }
            Err(error) => (None, Some(error.to_string())),
        };
        let changed = self.applications != next || self.application_error != error;
        self.applications = next;
        self.application_error = error;
        self.applications_received = self.applications.as_ref().map(|_| Instant::now());
        changed
    }

    fn saved_assignments(
        &self,
        mut view: Div,
        snapshot: &Snapshot,
        source: librewave_core::SourceId,
        cx: &mut Context<Self>,
    ) -> Div {
        for (index, rule) in snapshot.mixer.profile.assignments.iter().enumerate() {
            let (kind, name) = match rule.application {
                ApplicationId::Desktop(name) => ("Desktop ID", name),
                ApplicationId::Executable(name) => ("Executable", name),
            };
            let command = Command::SetApplicationAssignment {
                expected_generation: snapshot.mixer.generation(),
                application: rule.application,
                source: None,
            };
            view = view.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_MD))
                    .child(div().flex_1().child(name.as_str().to_owned()))
                    .child(caption(kind))
                    .child(caption(
                        snapshot
                            .mixer
                            .profile
                            .source(rule.source)
                            .map_or("System", |s| s.name.as_str())
                            .to_owned(),
                    ))
                    .child(action_button(
                        &format!("move-rule-{index}"),
                        "Move here",
                        Tone::Quiet,
                        !self.pending && rule.source != source,
                        cx,
                        {
                            let application = rule.application;
                            let generation = snapshot.mixer.generation();
                            move |this, _, cx| {
                                this.send(
                                    Command::SetApplicationAssignment {
                                        expected_generation: generation,
                                        application,
                                        source: Some(source),
                                    },
                                    cx,
                                );
                            }
                        },
                    ))
                    .child(icon_button(
                        &format!("forget-app-{index}"),
                        "x",
                        "Forget assignment",
                        Tone::Quiet,
                        INK,
                        !self.pending,
                        cx,
                        move |this, _, cx| this.send(command, cx),
                    )),
            );
        }
        view
    }

    pub(super) fn application_panel(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Div {
        let mut view = div().flex().flex_col().gap(px(SPACE_MD));
        let Some(source) = self.application_input else {
            return view;
        };
        let Some(input) = snapshot.mixer.profile.source(source) else {
            return view;
        };
        view = view.child(caption(format!("Manage {}", input.name)));
        view = view.child(self.channel_controls(snapshot, source, cx));
        view = self.saved_assignments(view, snapshot, source, cx);
        let Some(ApplicationSnapshot::Ready {
            generation: _,
            applications,
            unidentified_streams,
            movement,
            route,
        }) = &self.applications
        else {
            return view.child(caption(self.application_error.clone().unwrap_or_else(|| {
                if self.audio_active() {
                    "Waiting for application observations.".to_owned()
                } else {
                    "Start the mixer to detect playback applications.".to_owned()
                }
            })));
        };
        let busy = matches!(
            route,
            Some(ApplicationRoute::Pending { .. } | ApplicationRoute::Restoring { .. })
        );
        let allowed = *movement == ApplicationMovement::Allowed
            && !busy
            && !self.pending
            && self.audio_active()
            && self
                .applications_received
                .is_some_and(|t| t.elapsed() <= Duration::from_millis(1500));
        if *movement != ApplicationMovement::Allowed {
            view = view.child(caption("WirePlumber stream movement is disabled or unavailable."));
        }
        if applications.is_empty() {
            view = view.child(caption("No playback applications detected."));
        }
        for (index, application) in applications.iter().enumerate() {
            let command = Command::SetApplicationAssignment {
                expected_generation: snapshot.mixer.generation(),
                application: application.id,
                source: Some(source),
            };
            view = view.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_MD))
                    .child(div().flex_1().child(application.name.as_str().to_owned()))
                    .child(caption(format!("{} stream(s)", application.streams)))
                    .child(icon_button(
                        &format!("assign-app-{index}"),
                        "plus",
                        if application.move_blocked {
                            "Application forbids movement"
                        } else {
                            "Remember assignment"
                        },
                        Tone::Quiet,
                        INK,
                        allowed
                            && !application.move_blocked
                            && !snapshot
                                .mixer
                                .profile
                                .assignments
                                .iter()
                                .any(|r| r.application == application.id && r.source == source),
                        cx,
                        move |this, _, cx| {
                            this.send(command, cx);
                        },
                    )),
            );
        }
        if *unidentified_streams > 0 {
            view = view.child(caption(format!(
                "{unidentified_streams} stream(s) have no verified application identity."
            )));
        }
        if let Some(route) = route {
            view = view.child(div().text_color(gpui::rgb(FAINT)).child(match route {
                ApplicationRoute::Pending { .. } => {
                    "Waiting for WirePlumber audio links.".to_owned()
                }
                ApplicationRoute::Verified { .. } => "Application audio links verified.".to_owned(),
                ApplicationRoute::Restoring { .. } => {
                    "Restoring prior application targets.".to_owned()
                }
                ApplicationRoute::Failed { reason, .. } => reason.clone(),
            }));
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::shell_with;
    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn channel_panel_discards_drafts_and_reopens_from_saved_state(cx: &mut gpui::TestAppContext) {
        let mut snapshot = Snapshot::empty();
        snapshot.mixer.profile = snapshot
            .mixer
            .profile
            .with_input(InputName::try_new("Music").expect("valid name"))
            .expect("available input");
        let source = snapshot.mixer.profile.sources.last().expect("added input").controls.source();
        let (view, cx) = cx.add_window_view(|_, _| shell_with(snapshot));
        for _ in 0..3 {
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.open_channel(source, cx);
                    let field = view.channel_name.as_ref().expect("name editor").clone();
                    field.update(cx, |field, cx| {
                        gpui::EntityInputHandler::replace_text_in_range(
                            field, None, "Unsaved", window, cx,
                        );
                    });
                    assert_eq!(field.read(cx).value(), "Unsaved");
                    view.close_channel(cx);
                    assert!(view.channel_name.is_none());
                    assert!(view.assignment_name.is_none());
                    view.open_channel(source, cx);
                    assert_eq!(
                        view.channel_name.as_ref().expect("name editor").read(cx).value(),
                        "Music"
                    );
                    view.close_channel(cx);
                });
            });
        }
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn invalid_rule_keeps_draft_without_starting_request(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.open_channel(SourceId::new(2), cx);
                view.save_channel_text(true, cx);
                assert!(view.action_error.is_some());
                assert!(!view.pending);
                assert!(view.assignment_name.is_some());
                assert!(view.channel_name.is_none());
            });
        });
    }

    #[test]
    fn late_application_reads_cannot_restore_a_previous_session_and_errors_are_visible() {
        let mut snapshot = Snapshot::empty();
        snapshot.mixer.runtime = librewave_core::MixerRuntimeState::Active;
        let mut shell = shell_with(snapshot);
        shell.request_epoch = 2;
        assert!(!shell.apply_application_result(
            1,
            Ok(Response::Applications {
                applications: ApplicationSnapshot::Unavailable {
                    reason: librewave_core::MixerInactiveReason::Starting
                }
            })
        ));
        assert!(shell.applications.is_none());
        assert!(shell.apply_application_result(
            2,
            Ok(Response::Mixer { mixer: librewave_core::MixerSnapshot::default() })
        ));
        assert!(shell.application_error.as_ref().is_some_and(|s| s.contains("unexpected")));
    }
}
