//! Active application selection stays on the versioned daemon interface.
use super::{
    Shell,
    controls::{Tone, caption, icon_button},
    theme::{FAINT, INK, SPACE_MD},
};
use gpui::{Context, Div, div, prelude::*, px};
use librewave_core::{
    ApplicationMovement, ApplicationRoute, ApplicationSnapshot, Command, Snapshot,
};
use librewave_ipc::Response;
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};
use std::time::{Duration, Instant};

impl Shell {
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
        for (index, rule) in
            snapshot.mixer.profile.assignments.iter().filter(|r| r.source == source).enumerate()
        {
            let name = match rule.application {
                librewave_core::ApplicationId::Desktop(name)
                | librewave_core::ApplicationId::Executable(name) => name,
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
                    .child(caption("Saved"))
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
        view = view.child(caption(format!("Remember applications for {}", input.name)));
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
                    "Start audio to detect playback applications.".to_owned()
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
