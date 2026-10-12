//! The Settings dialog: audio, setup, and the daemon connection.
use super::chrome::error_banner;
use super::controls::{
    Tone, action_button, activate, button_face, caption, chip, dot, icon, icon_button, icon_tile,
    labeled_icon_button,
};
use super::theme::{
    BACKGROUND, CAPTION, CAPTION_LINE, FAINT, GROUP, HEADING, HEADING_LINE, ICON_SMALL, INK, LINE,
    MONITOR, MONO, MUTED, PANEL, RADIUS, RADIUS_CARD, RAISED, SPACE_LG, SPACE_MD, SPACE_SM,
    SPACE_XL, SPACE_XS, TITLE, TITLE_LINE, WARNING,
};
use super::{SettingsModal, Shell, configured_socket_path};
use gpui::{
    AnyElement, BoxShadow, Context, Div, FontWeight, IntoElement, div, point, prelude::*, px, rgb,
    rgba,
};
use librewave_core::{Command, MixerRuntimeState, ServiceState};

/// One page of the Settings dialog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SettingsPane {
    Audio,
    Setup,
    Connection,
}

impl SettingsPane {
    const ALL: [Self; 3] = [Self::Audio, Self::Setup, Self::Connection];

    const fn identity(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Audio => ("pane-audio", "speaker-high", "Audio"),
            Self::Setup => ("pane-setup", "wrench", "Setup"),
            Self::Connection => ("pane-connection", "plugs-connected", "Connection"),
        }
    }
}

impl Shell {
    pub(super) fn settings_overlay(
        &self,
        modal: &SettingsModal,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let close = icon_button(
            "close-settings",
            "x",
            "Close",
            Tone::Quiet,
            MUTED,
            true,
            cx,
            Shell::close_settings,
        )
        .track_focus(&modal.close_focus);
        let (_, _, title) = self.settings_pane.identity();
        let pane = match self.settings_pane {
            SettingsPane::Audio => self.audio_pane(cx),
            SettingsPane::Setup => self.setup_pane(cx),
            SettingsPane::Connection => self.connection_pane(cx),
        };
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x0000_00B8))
            .p(px(SPACE_XL))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("settings-panel")
                    .debug_selector(|| "settings-panel".to_owned())
                    .w(px(760.))
                    .h(px(520.))
                    .max_h_full()
                    .track_focus(&modal.focus)
                    .tab_group()
                    .tab_index(0)
                    .tab_stop(false)
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                        this.close_settings(window, cx);
                        cx.stop_propagation();
                    }))
                    .rounded(px(RADIUS_CARD))
                    .overflow_hidden()
                    .bg(rgb(PANEL))
                    .shadow(vec![BoxShadow {
                        color: gpui::black().opacity(0.6),
                        offset: point(px(0.), px(24.)),
                        blur_radius: px(64.),
                        spread_radius: px(0.),
                    }])
                    .flex()
                    .child(self.settings_navigation(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(pane_header(title, self.settings_pane, close))
                            .child(
                                div()
                                    .id("settings-pane")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .p(px(SPACE_XL))
                                    .flex()
                                    .flex_col()
                                    .gap(px(SPACE_LG))
                                    .when_some(self.action_error.clone(), |view, error| {
                                        view.child(error_banner(error, true, cx))
                                    })
                                    .child(pane),
                            ),
                    ),
            )
    }

    fn settings_navigation(&self, cx: &mut Context<Self>) -> Div {
        let mut navigation = div()
            .w(px(200.))
            .h_full()
            .flex_shrink_0()
            .p(px(SPACE_MD))
            .bg(rgb(BACKGROUND))
            .flex()
            .flex_col()
            .gap(px(SPACE_XS))
            .child(
                div()
                    .px(px(SPACE_MD))
                    .pt(px(SPACE_SM))
                    .pb(px(SPACE_MD))
                    .text_size(px(CAPTION))
                    .line_height(px(CAPTION_LINE))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(FAINT))
                    .child("Settings"),
            );
        for pane in SettingsPane::ALL {
            let (id, glyph, name) = pane.identity();
            let selected = pane == self.settings_pane;
            navigation = navigation.child(activate(
                button_face(id, Tone::Quiet)
                    .w_full()
                    .h(px(36.))
                    .justify_start()
                    .gap(px(SPACE_MD))
                    .when(selected, |view| view.bg(rgb(RAISED)).text_color(rgb(INK)))
                    .child(icon(glyph, if selected { INK } else { MUTED }, ICON_SMALL))
                    .child(name),
                true,
                cx,
                move |this, _, cx| {
                    this.settings_pane = pane;
                    cx.notify();
                },
            ));
        }
        navigation
    }

    fn audio_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = !self.pending;
        let runtime = self.snapshot.as_ref().map(|snapshot| snapshot.mixer.runtime);
        let running = matches!(runtime, Some(MixerRuntimeState::Active));
        let (glyph, color, state, detail) = match runtime {
            Some(MixerRuntimeState::Active) => (
                "speaker-high",
                MONITOR,
                "Audio is running",
                "Closing the window keeps audio running.".to_owned(),
            ),
            Some(MixerRuntimeState::Inactive { reason }) => {
                ("power", MUTED, "Audio is stopped", sentence(&reason.to_string()))
            }
            None => (
                "power",
                MUTED,
                "Audio state is unavailable",
                "Connect to the LibreWave service to check audio state.".to_owned(),
            ),
        };
        let start = labeled_icon_button(
            "session-start",
            "play",
            "Start audio",
            Tone::Primary,
            enabled,
            cx,
            Shell::start_audio,
        );
        let stop = labeled_icon_button(
            "session-stop",
            "stop",
            "Stop and restore",
            Tone::Secondary,
            enabled,
            cx,
            |this, _, cx| this.run_lifecycle(librewavectl::SessionAction::Stop, cx),
        );
        div()
            .flex()
            .flex_col()
            .gap(px(SPACE_LG))
            .child(
                group().child(
                    div()
                        .p(px(SPACE_LG))
                        .flex()
                        .flex_col()
                        .gap(px(SPACE_LG))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(SPACE_MD))
                                .child(icon_tile(glyph, color, 44.))
                                .child(description(state, detail)),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(SPACE_SM))
                                .when(!running, |view| view.child(start))
                                .child(stop),
                        ),
                ),
            )
            .child(self.conflict_row(cx))
            .children(self.outcome())
            .into_any_element()
    }

    /// The `WirePlumber` file that start suspends while audio runs.
    fn conflict_row(&self, cx: &mut Context<Self>) -> Div {
        let enabled = !self.pending;
        let chosen = self.suspend_config.as_ref().map(|path| path.display().to_string());
        group().child(
            row(
                "WirePlumber conflict",
                chosen.clone().unwrap_or_else(|| {
                    "If start reports a conflicting file, choose it here. LibreWave \
                     backs it up and restores it when audio stops."
                        .to_owned()
                }),
            )
            .child(labeled_icon_button(
                "choose-fragment",
                "folder-open",
                "Choose file…",
                Tone::Secondary,
                enabled,
                cx,
                |this, _, cx| this.choose_fragment(cx),
            ))
            .when(chosen.is_some(), |view| {
                view.child(icon_button(
                    "clear-fragment",
                    "x",
                    "Clear the chosen file",
                    Tone::Quiet,
                    MUTED,
                    enabled,
                    cx,
                    |this, _, cx| {
                        this.suspend_config = None;
                        cx.notify();
                    },
                ))
            }),
        )
    }

    fn setup_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut commands = group();
        for (index, (name, detail, command)) in [
            ("Install or repair", "Installs the service and the device access rule.", "setup"),
            ("Check the installation", "Reports each installed part and its state.", "doctor"),
            ("Remove", "Removes every file that setup installed.", "uninstall"),
        ]
        .into_iter()
        .enumerate()
        {
            commands = commands.when(index > 0, |view| view.child(separator())).child(
                row(name, detail.to_owned()).child(code(&format!("librewavectl {command}"))),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(SPACE_LG))
            .child(
                group().child(
                    row(
                        "Setup plan",
                        "Lists the files and the access rule that setup installs. \
                         This makes no changes."
                            .to_owned(),
                    )
                    .child(action_button(
                        "setup-plan",
                        "Show plan",
                        Tone::Secondary,
                        !self.pending,
                        cx,
                        |this, _, cx| {
                            this.run_lifecycle(librewavectl::SessionAction::SetupPlan, cx);
                        },
                    )),
                ),
            )
            .children(self.outcome())
            .child(caption("Run these commands in a terminal.").text_color(rgb(FAINT)))
            .child(commands)
            .into_any_element()
    }

    fn connection_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let service = |name: &'static str, state: Option<&ServiceState>| {
            let (color, text) = match state {
                Some(ServiceState::Available) => (MONITOR, "Available".to_owned()),
                Some(ServiceState::Unavailable { reason }) => {
                    (WARNING, sentence(&reason.to_string()))
                }
                None => (FAINT, "Unknown".to_owned()),
            };
            div()
                .min_h(px(52.))
                .px(px(SPACE_LG))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(SPACE_LG))
                .child(name)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(SPACE_SM))
                        .child(dot(color))
                        .child(div().text_color(rgb(MUTED)).child(text)),
                )
        };
        let audio = self.snapshot.as_ref().map(|snapshot| &snapshot.audio);
        let (color, state) =
            if self.snapshot.is_some() { (MONITOR, "Connected") } else { (FAINT, "Not connected") };
        div()
            .flex()
            .flex_col()
            .gap(px(SPACE_LG))
            .child(
                group().child(
                    div()
                        .p(px(SPACE_LG))
                        .flex()
                        .items_center()
                        .gap(px(SPACE_MD))
                        .child(icon_tile(
                            if self.snapshot.is_some() { "plugs-connected" } else { "plugs" },
                            color,
                            44.,
                        ))
                        .child(description("LibreWave service", state.to_owned()))
                        .child(icon_button(
                            "refresh",
                            "arrow-clockwise",
                            "Check the connection again",
                            Tone::Secondary,
                            INK,
                            !self.pending,
                            cx,
                            |this, _, cx| this.send(Command::GetStatus, cx),
                        )),
                ),
            )
            .child(
                group().child(
                    row("Socket", "The service answers on this path.".to_owned())
                        .child(code(&configured_socket_path().display().to_string())),
                ),
            )
            .child(
                group()
                    .child(service("PipeWire", audio.map(|audio| &audio.pipewire)))
                    .child(separator())
                    .child(service("WirePlumber", audio.map(|audio| &audio.wireplumber))),
            )
            .when_some(self.connection_error.clone(), |view, error| {
                view.child(error_banner(error, false, cx))
            })
            .into_any_element()
    }

    /// The result of the last audio or setup action.
    fn outcome(&self) -> Vec<Div> {
        let mut outcome = Vec::new();
        if let Some(message) = self.lifecycle_message.clone() {
            outcome.push(
                group()
                    .p(px(SPACE_LG))
                    .text_size(px(CAPTION))
                    .line_height(px(18.))
                    .text_color(rgb(MUTED))
                    .font_family(MONO)
                    .child(message),
            );
        }
        outcome
    }
}

/// The page title and the close button.
fn pane_header(title: &'static str, pane: SettingsPane, close: gpui::Stateful<Div>) -> Div {
    div()
        .pl(px(SPACE_XL))
        .pr(px(SPACE_MD))
        .pt(px(SPACE_MD))
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(SPACE_MD))
                .child(
                    div()
                        .text_size(px(TITLE))
                        .line_height(px(TITLE_LINE))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .when(pane == SettingsPane::Audio, |view| {
                    view.child(chip("Experimental", WARNING))
                }),
        )
        .child(close)
}

/// A filled surface that holds related rows.
fn group() -> Div {
    div().w_full().rounded(px(RADIUS)).bg(rgb(GROUP)).flex().flex_col()
}

fn separator() -> Div {
    div().mx(px(SPACE_LG)).h(px(1.)).bg(rgb(LINE))
}

fn description(title: &str, detail: String) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .text_size(px(HEADING))
                .line_height(px(HEADING_LINE))
                .font_weight(FontWeight::MEDIUM)
                .child(title.to_owned()),
        )
        .child(caption(detail))
}

/// A setting: its name and purpose on the left, its controls after it.
fn row(title: &str, detail: String) -> Div {
    div().p(px(SPACE_LG)).flex().items_center().gap(px(SPACE_MD)).child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(div().font_weight(FontWeight::MEDIUM).child(title.to_owned()))
            .child(caption(detail)),
    )
}

fn code(text: &str) -> Div {
    div()
        .flex_shrink_0()
        .px(px(SPACE_SM))
        .py(px(SPACE_XS))
        .rounded(px(6.))
        .bg(rgb(BACKGROUND))
        .font_family(MONO)
        .text_size(px(CAPTION))
        .line_height(px(CAPTION_LINE))
        .text_color(rgb(INK))
        .child(text.to_owned())
}

/// Starts a daemon status phrase with a capital letter and ends it with a period.
fn sentence(phrase: &str) -> String {
    let mut characters = phrase.chars();
    characters.next().map_or_else(String::new, |first| {
        let rest = characters.as_str();
        let end = if rest.ends_with('.') { "" } else { "." };
        format!("{}{rest}{end}", first.to_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_phrases_become_sentences_without_changing_their_words() {
        assert_eq!(sentence("not running"), "Not running.");
        assert_eq!(sentence("PipeWire must be available."), "PipeWire must be available.");
        assert_eq!(sentence(""), "");
    }
}
