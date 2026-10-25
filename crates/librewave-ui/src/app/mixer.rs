//! The software mixer: one strip for each input with a fader for each mix.
use super::Shell;
use super::controls::{
    Tone, activate, button_face, caption, card, icon, icon_button, icon_tile, identified, label,
    tabular, tip,
};
use super::theme::{
    CAPTION, DANGER, FAINT, FOCUS, HEADING, HEADING_LINE, ICON_SMALL, INK, LINE, MONITOR, MUTED,
    PANEL, RADIUS, RADIUS_CARD, RAISED, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, STREAM,
    alpha,
};
use gpui::{
    Bounds, Context, Div, DragMoveEvent, FontWeight, MouseButton, Stateful, canvas, div,
    prelude::*, px, relative, rgb, rgba,
};
use librewave_core::{
    Command, FaderGain, MixRoute, MixTarget, MixerGeneration, MixerSourceSnapshot, Snapshot,
    SourceId, SourceRole,
};

/// A transient pointer intent, never an authoritative or persisted mixer state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FaderEdit {
    pub(super) source: SourceId,
    pub(super) target: MixTarget,
    pub(super) fader: FaderGain,
}

/// One latest pointer intent across all channel and output faders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MixerEdit {
    Input(FaderEdit),
    Output { target: MixTarget, fader: FaderGain },
}
impl From<FaderEdit> for MixerEdit {
    fn from(edit: FaderEdit) -> Self {
        Self::Input(edit)
    }
}
impl MixerEdit {
    pub(super) fn command(self, snapshot: &Snapshot) -> Option<Command> {
        match self {
            Self::Input(edit) => edit.command(snapshot),
            Self::Output { target, fader } => {
                let route = snapshot.mixer.profile.outputs.route(target);
                (route.fader() != fader).then_some(Command::SetMixerOutput {
                    expected_generation: snapshot.mixer.generation(),
                    target,
                    route: MixRoute::new(route.enabled(), fader),
                })
            }
        }
    }
    fn input(self) -> Option<FaderEdit> {
        match self {
            Self::Input(edit) => Some(edit),
            Self::Output { .. } => None,
        }
    }
}

impl FaderEdit {
    pub(super) fn command(self, snapshot: &Snapshot) -> Option<Command> {
        let controls = snapshot.mixer.profile.source(self.source)?.controls;
        let route = match self.target {
            MixTarget::Monitor => controls.monitor(),
            MixTarget::Stream => controls.stream(),
        };
        if self.fader == route.fader() {
            return None;
        }
        Some(Command::SetMixerRoute {
            expected_generation: snapshot.mixer.generation(),
            source: self.source,
            target: self.target,
            route: MixRoute::new(route.enabled(), self.fader),
        })
    }
}

/// A channel that is being dragged to a new position.
#[derive(Clone)]
pub(super) struct ChannelDrag {
    source: SourceId,
    name: gpui::SharedString,
}

/// The request that moves a channel to a strip position. A drop on the
/// channel's own strip, or a move past either end, changes nothing.
fn channel_move(snapshot: &Snapshot, source: SourceId, target: usize) -> Option<Command> {
    let sources = &snapshot.mixer.profile.sources;
    let current = sources.iter().position(|s| s.controls.source() == source)?;
    let target = target.min(sources.len().checked_sub(1)?);
    (target != current).then_some(Command::MoveMixerInput {
        expected_generation: snapshot.mixer.generation(),
        source,
        position: u16::try_from(target).ok()?,
    })
}

/// The card that follows the pointer while a channel is dragged.
struct ChannelGhost {
    name: gpui::SharedString,
}

impl gpui::Render for ChannelGhost {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(SPACE_MD))
            .h(px(36.))
            .rounded(px(RADIUS))
            .bg(rgb(RAISED))
            .opacity(0.9)
            .flex()
            .items_center()
            .gap(px(SPACE_SM))
            .font_family("Geist")
            .text_size(px(14.))
            .text_color(rgb(INK))
            .child(icon("dots-six", MUTED, ICON_SMALL))
            .child(self.name.clone())
    }
}

#[derive(Clone)]
struct FaderDrag {
    source: SourceId,
    target: MixTarget,
}

// Strips keep one width, so a longer name or source never moves its neighbors.
const STRIP_WIDTH: f32 = 212.;
const LANE_WIDTH: f32 = 52.;
const SCALE_WIDTH: f32 = 28.;
const FADER_WIDTH: f32 = 44.;
const CAP: f32 = 16.;
const FADER_MIN: f32 = 80.;
// The level readout and mute button under each fader, with their gaps.
const LANE_FOOT: f32 = SPACE_MD + 24. + SPACE_MD + 32.;
// The inputs keep this height when the window scrolls instead of filling it.
const INPUTS_MIN: f32 = 360.;
const INPUTS_COMPACT: f32 = 440.;
/// The outputs label and its two rows.
const OUTPUTS_HEIGHT: f32 = 16. + SPACE_MD + 2. * 60. + SPACE_SM;
/// The mixer area scrolls below this height instead of shrinking its faders.
pub(super) const MIXER_MIN_HEIGHT: f32 = INPUTS_MIN + SPACE_XL + OUTPUTS_HEIGHT;
// Levels named on the scale between the two faders of a strip.
const SCALE: [i16; 5] = [12, 0, -20, -40, -60];

/// The icon for an input: its role, a kind that its name states, or one of
/// the remaining glyphs, chosen by identity so neighbors differ.
pub(super) fn input_glyph(source: &MixerSourceSnapshot) -> &'static str {
    const NAMED: [(&[&str], &str); 8] = [
        (&["music", "song", "spotify"], "music-notes"),
        (&["browser", "web", "firefox", "chrome"], "globe-simple"),
        (&["chat", "voice", "discord", "call", "talk"], "chats-circle"),
        (&["game", "steam"], "game-controller"),
        (&["video", "movie", "film", "youtube", "twitch"], "film-strip"),
        (&["sfx", "effect", "sound", "alert", "notification"], "bell-simple"),
        (&["stream", "broadcast", "obs"], "broadcast"),
        (&["podcast", "radio"], "radio"),
    ];
    const SPARE: [&str; 4] = ["waveform", "sparkle", "megaphone-simple", "user-sound"];
    match source.role {
        SourceRole::Microphone => return "mic",
        SourceRole::System => return "system",
        SourceRole::Software => {}
    }
    let name = source.name.to_lowercase();
    NAMED.iter().find(|(words, _)| words.iter().any(|word| name.contains(word))).map_or_else(
        || SPARE[usize::from(source.controls.source().get()) % SPARE.len()],
        |(_, glyph)| glyph,
    )
}

/// The narrowest mixer beside the stage. Narrower strips scroll sideways.
pub(super) fn mixer_min_width() -> f32 {
    2. * STRIP_WIDTH + SPACE_MD
}

const fn mix_identity(target: MixTarget) -> (u32, &'static str, &'static str) {
    match target {
        MixTarget::Monitor => (MONITOR, "headphones", "Monitor"),
        MixTarget::Stream => (STREAM, "stream", "Stream"),
    }
}

impl Shell {
    pub(super) fn mixer_section(
        &self,
        snapshot: &Snapshot,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mut strips = div()
            .id("input-strips")
            .w_full()
            .flex_1()
            .min_h_0()
            .flex()
            .gap(px(SPACE_MD))
            .overflow_x_scroll();
        for (position, source) in snapshot.mixer.profile.sources.iter().enumerate() {
            strips =
                strips.child(self.channel_strip(source, position, snapshot.mixer.generation(), cx));
        }
        let strips = strips.child(self.add_input(snapshot, cx));
        div()
            .id("mixer")
            .debug_selector(|| "mixer".to_owned())
            .w_full()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(SPACE_XL))
            .child(
                div()
                    .when(compact, |view| view.h(px(INPUTS_COMPACT)))
                    .when(!compact, |view| view.flex_1().min_h(px(INPUTS_MIN)))
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_MD))
                    .child(label("Inputs"))
                    .child(strips),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_MD))
                    .child(label("Outputs"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(SPACE_SM))
                            .child(self.output_row(
                                MixTarget::Monitor,
                                "headphones",
                                "Wave:3 headphones",
                                self.meters.as_ref().map(|f| f.outputs[1]),
                                self.meter_error.as_deref(),
                                snapshot,
                                cx,
                            ))
                            .child(self.output_row(
                                MixTarget::Stream,
                                "system",
                                "Source for applications",
                                self.meters.as_ref().map(|f| f.outputs[2]),
                                self.meter_error.as_deref(),
                                snapshot,
                                cx,
                            )),
                    ),
            )
    }

    fn channel_strip(
        &self,
        source: &MixerSourceSnapshot,
        position: usize,
        generation: MixerGeneration,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = source.controls.source();
        card()
            .drag_over::<ChannelDrag>(|style, _, _, _| style.bg(rgb(RAISED)))
            .on_drop(cx.listener(move |this, drag: &ChannelDrag, _, cx| {
                this.move_channel(drag.source, position, cx);
            }))
            .child(self.drag_handle(source, position, cx))
            .w(px(STRIP_WIDTH))
            .flex_shrink_0()
            .relative()
            .p(px(SPACE_LG))
            .flex()
            .flex_col()
            .gap(px(SPACE_XL))
            .child(self.source_heading(source, generation, cx))
            .when(self.application_input == Some(id), |view| {
                // The pop-up opens under the heading and takes no room in the strip.
                view.child(
                    div().absolute().top(px(SPACE_LG + 40. + SPACE_SM)).left(px(SPACE_SM)).child(
                        self.snapshot.as_ref().map_or_else(
                            || div().into_any_element(),
                            |snapshot| self.channel_menu(snapshot, id, cx),
                        ),
                    ),
                )
            })
            .child(super::meters::bar(
                &format!("input-{}", source.controls.source()),
                self.meters.as_ref().and_then(|frame| {
                    frame
                        .sources
                        .iter()
                        .find(|meter| meter.source == source.controls.source())
                        .map(|meter| meter.levels)
                }),
                INK,
                self.meter_error.as_deref(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_MD))
                    .child(self.mix_header(source, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .justify_center()
                            .gap(px(SPACE_XS))
                            .child(self.lane(source, MixTarget::Monitor, generation, cx))
                            .child(scale())
                            .child(self.lane(source, MixTarget::Stream, generation, cx)),
                    ),
            )
    }

    fn source_heading(
        &self,
        source: &MixerSourceSnapshot,
        generation: MixerGeneration,
        cx: &mut Context<Self>,
    ) -> Div {
        let microphone = source.role == SourceRole::Microphone;
        let id = source.controls.source();
        let removable = !self.pending
            && self.settings_modal.is_none()
            && self.snapshot.as_ref().is_some_and(|s| input_layout_available(s.mixer.runtime));
        div()
            .flex()
            .items_center()
            .gap(px(SPACE_SM))
            .child(icon_tile(input_glyph(source), INK, 36.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(HEADING))
                            .line_height(px(HEADING_LINE))
                            .font_weight(FontWeight::MEDIUM)
                            .child(source.name.clone()),
                    )
                    .child(
                        caption(if microphone {
                            "Wave:3"
                        } else if source.role == SourceRole::System {
                            "Desktop audio"
                        } else {
                            "Application audio"
                        })
                        .truncate(),
                    ),
            )
            .when(!microphone, |view| {
                view.child(
                    icon_button(
                        &format!("applications-{id}"),
                        "system",
                        "Manage application assignments",
                        Tone::Quiet,
                        FAINT,
                        !self.pending,
                        cx,
                        move |this, _, cx| {
                            this.open_channel(id, cx);
                            this.poll_applications(cx);
                            cx.notify();
                        },
                    )
                    .size(px(28.)),
                )
            })
            .when(source.role == SourceRole::Software, |view| {
                view.child(
                    icon_button(
                        &format!("remove-input-{id}"),
                        "x",
                        if removable {
                            "Remove input"
                        } else {
                            "Wait for mixer readiness before removing an input"
                        },
                        Tone::Quiet,
                        FAINT,
                        removable,
                        cx,
                        move |this, _, cx| {
                            this.send(
                                Command::RemoveMixerInput {
                                    expected_generation: generation,
                                    source: id,
                                },
                                cx,
                            );
                        },
                    )
                    .size(px(28.)),
                )
            })
    }

    /// The dots at the top of a strip. Dragging them moves the channel, and
    /// the arrow keys move it by one place when they have focus.
    fn drag_handle(
        &self,
        source: &MixerSourceSnapshot,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = source.controls.source();
        let enabled = !self.pending && self.settings_modal.is_none();
        let drag = ChannelDrag { source: id, name: source.name.as_str().to_owned().into() };
        let handle = identified(&format!("drag-channel-{}", id.get()))
            .absolute()
            .top(px(2.))
            .left(px(STRIP_WIDTH / 2. - 24.))
            .w(px(48.))
            .h(px(12.))
            .rounded(px(4.))
            .border_1()
            .border_color(rgba(0))
            .flex()
            .items_center()
            .justify_center()
            .focusable()
            .tab_index(0)
            .tab_stop(enabled)
            .focus_visible(|style| style.border_color(rgb(FOCUS)))
            .child(icon("dots-six", FAINT, ICON_SMALL))
            .tooltip(tip("Drag to move this channel"));
        if !enabled {
            return handle;
        }
        handle
            .cursor_grab()
            .hover(|style| style.bg(rgb(RAISED)))
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| ChannelGhost { name: drag.name.clone() }))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                let target = match event.keystroke.key.as_str() {
                    "left" => position.checked_sub(1),
                    "right" => Some(position + 1),
                    _ => return,
                };
                cx.stop_propagation();
                if let Some(target) = target {
                    this.move_channel(id, target, cx);
                }
            }))
    }

    /// Moves a channel to a strip position in one daemon request.
    pub(super) fn move_channel(&mut self, source: SourceId, target: usize, cx: &mut Context<Self>) {
        if let Some(command) =
            self.snapshot.as_ref().and_then(|snapshot| channel_move(snapshot, source, target))
        {
            self.send(command, cx);
        }
    }

    /// The two mix icons over their faders, with the link control between them.
    fn mix_header(&self, source: &MixerSourceSnapshot, cx: &mut Context<Self>) -> Div {
        let id = source.controls.source();
        let name = |target| {
            let (color, glyph, _) = mix_identity(target);
            let route = match target {
                MixTarget::Monitor => source.controls.monitor(),
                MixTarget::Stream => source.controls.stream(),
            };
            let color = if route.enabled() { color } else { FAINT };
            identified(&format!("mix-{}-{target}", id.get()))
                .w(px(LANE_WIDTH))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .child(icon(glyph, color, ICON_SMALL))
                .tooltip(tip(match target {
                    MixTarget::Monitor => "Monitor mix",
                    MixTarget::Stream => "Stream mix",
                }))
        };
        let link = icon_button(
            &format!("linked-faders-{id}"),
            if self.linked_faders.contains(&id) { "plugs-connected" } else { "plugs" },
            "Link or unlink channel fader levels in this window",
            Tone::Quiet,
            if self.linked_faders.contains(&id) { INK } else { FAINT },
            !self.pending && self.settings_modal.is_none(),
            cx,
            move |this, _, cx| {
                if !this.linked_faders.remove(&id) {
                    this.linked_faders.insert(id);
                }
                cx.notify();
            },
        );
        div()
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(SPACE_XS))
            .child(name(MixTarget::Monitor))
            .child(div().w(px(SCALE_WIDTH)).flex().justify_center().child(link.size(px(28.))))
            .child(name(MixTarget::Stream))
    }

    /// One source in one mix: the fader, level, and mute.
    fn lane(
        &self,
        source: &MixerSourceSnapshot,
        target: MixTarget,
        generation: MixerGeneration,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = source.controls.source();
        let route = match target {
            MixTarget::Monitor => source.controls.monitor(),
            MixTarget::Stream => source.controls.stream(),
        };
        let (color, _, _) = mix_identity(target);
        let key = format!("fader-{}-{target}", id.get());
        let shown = self
            .queued_fader
            .or(self.in_flight_fader)
            .and_then(MixerEdit::input)
            .filter(|edit| {
                edit.source == id && (edit.target == target || self.linked_faders.contains(&id))
            })
            .map_or(route.fader(), |edit| edit.fader);
        let open = self.settings_modal.is_none();
        let muted = !route.enabled();
        let toggle = route_toggle_command(id, target, route, generation);
        div()
            .w(px(LANE_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(SPACE_MD))
            .child(fader_track(
                &key,
                id,
                target,
                route,
                generation,
                shown,
                if muted { FAINT } else { color },
                open && (!self.pending || self.in_flight_fader.is_some()),
                cx,
            ))
            .child(
                // A busy client ignores the request and a level at its target has
                // no command, so this readout does not flicker while dragging.
                activate(
                    button_face(&format!("{key}-reset"), Tone::Quiet)
                        .h(px(24.))
                        .w_full()
                        .px_0()
                        .text_size(px(CAPTION))
                        .font_features(tabular())
                        .text_color(rgb(if muted { FAINT } else { INK }))
                        .child(format!("{:.1} dB", shown.decibels()).replace('-', "−"))
                        .tooltip(tip("Reset to 0 dB")),
                    open,
                    cx,
                    move |this, _, cx| {
                        this.send_fader(
                            FaderEdit { source: id, target, fader: FaderGain::UNITY },
                            cx,
                        );
                    },
                ),
            )
            .child(
                icon_button(
                    &format!("{key}-toggle"),
                    if muted { "speaker-slash" } else { "speaker-high" },
                    match (target, muted) {
                        (MixTarget::Monitor, false) => "Mute in the Monitor mix",
                        (MixTarget::Monitor, true) => "Unmute in the Monitor mix",
                        (MixTarget::Stream, false) => "Mute in the Stream mix",
                        (MixTarget::Stream, true) => "Unmute in the Stream mix",
                    },
                    Tone::Secondary,
                    if muted { DANGER } else { MUTED },
                    open && !self.pending,
                    cx,
                    move |this, _, cx| this.send(toggle, cx),
                )
                .w(px(48.))
                .when(muted, |view| view.bg(rgba(alpha(DANGER, 0x24)))),
            )
    }
}

impl Shell {
    fn add_input(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Stateful<Div> {
        let free = (snapshot.mixer.profile.sources.len() < librewave_core::MAX_MIXER_SOURCES
            && snapshot.mixer.profile.next_source_id < u16::MAX)
            .then_some(snapshot.mixer.profile.next_source_id);
        let enabled = free.is_some()
            && !self.pending
            && self.settings_modal.is_none()
            && input_layout_available(snapshot.mixer.runtime);
        let generation = snapshot.mixer.generation();
        let name =
            free.and_then(|id| librewave_core::InputName::try_new(&format!("Input {id}")).ok());
        activate(
            identified("add-input")
                .w(px(STRIP_WIDTH))
                .flex_shrink_0()
                .rounded(px(RADIUS_CARD))
                .bg(rgba(alpha(PANEL, 0x8C)))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(SPACE_MD))
                .child(icon_tile("plus", MUTED, 40.))
                .child(div().text_color(rgb(FAINT)).child("Add input"))
                .tooltip(tip(if free.is_none() {
                    "All 12 input slots are in use"
                } else if !enabled {
                    "Wait for mixer readiness before adding an input"
                } else {
                    "Add a software input"
                })),
            enabled,
            cx,
            move |this, _, cx| {
                if let Some(name) = name {
                    this.send(Command::AddMixerInput { expected_generation: generation, name }, cx);
                }
            },
        )
    }
}

/// Software output controls with a read-only destination.
impl Shell {
    #[allow(
        clippy::too_many_arguments,
        reason = "output view shares the current profile and meter observation"
    )]
    fn output_row(
        &self,
        target: MixTarget,
        glyph: &'static str,
        destination: &'static str,
        levels: Option<librewave_core::StereoLevels>,
        error: Option<&str>,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Div {
        let (color, _, name) = mix_identity(target);
        let key = name.to_lowercase();
        let route = snapshot.mixer.profile.outputs.route(target);
        let shown = self
            .queued_fader
            .or(self.in_flight_fader)
            .and_then(|edit| match edit {
                MixerEdit::Output { target: current, fader } if current == target => Some(fader),
                _ => None,
            })
            .unwrap_or(route.fader());
        let generation = snapshot.mixer.generation();
        let controls_open = self.settings_modal.is_none();
        let mutable = controls_open && !self.pending;
        let identity = output_identity(target, glyph, destination, levels, error);
        let controls = div()
            .flex_1()
            .min_w(px(224.))
            .flex()
            .items_center()
            .gap(px(SPACE_MD))
            .child(output_track(
                &format!("output-{key}-level"),
                target,
                shown,
                color,
                controls_open,
                cx,
            ))
            .child(icon_button(
                &format!("output-{key}-mute"),
                if route.enabled() { "speaker-high" } else { "speaker-slash" },
                "Mute software output",
                Tone::Quiet,
                if route.enabled() { color } else { DANGER },
                mutable,
                cx,
                move |this, _, cx| {
                    this.send(
                        Command::SetMixerOutput {
                            expected_generation: generation,
                            target,
                            route: MixRoute::new(!route.enabled(), route.fader()),
                        },
                        cx,
                    );
                },
            ))
            .child(activate(
                button_face(&format!("output-{key}-reset"), Tone::Quiet)
                    .w(px(80.))
                    .justify_end()
                    .font_features(tabular())
                    .child(format!("{:.1} dB", shown.decibels()))
                    .tooltip(tip("Reset software output to 0 dB")),
                mutable,
                cx,
                move |this, _, cx| {
                    this.send_fader(MixerEdit::Output { target, fader: FaderGain::UNITY }, cx);
                },
            ));
        card()
            .w_full()
            .p(px(SPACE_MD))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(SPACE_MD))
            .child(identity)
            .child(controls)
    }
}

fn output_identity(
    target: MixTarget,
    glyph: &'static str,
    destination: &'static str,
    levels: Option<librewave_core::StereoLevels>,
    error: Option<&str>,
) -> Div {
    let (color, mix_glyph, name) = mix_identity(target);
    let key = name.to_lowercase();
    div()
        .flex_1()
        .min_w(px(320.))
        .flex()
        .items_center()
        .gap(px(SPACE_MD))
        .child(div().w(px(40.)).flex_shrink_0().child(super::meters::bar(
            &format!("output-{key}"),
            levels,
            color,
            error,
        )))
        .child(
            div()
                .w(px(152.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(SPACE_MD))
                .child(icon_tile(mix_glyph, color, 36.))
                .child(div().font_weight(FontWeight::MEDIUM).child(format!("{name} mix"))),
        )
        .child(
            identified(&format!("output-{key}-destination"))
                .flex_1()
                .min_w_0()
                .h(px(36.))
                .px(px(SPACE_MD))
                .rounded(px(RADIUS))
                .bg(rgb(RAISED))
                .flex()
                .items_center()
                .gap(px(SPACE_SM))
                .child(icon(glyph, MUTED, ICON_SMALL))
                .child(div().flex_1().min_w_0().truncate().child(destination))
                .tooltip(tip("Output selection is not available yet")),
        )
}

#[derive(Clone)]
struct OutputDrag {
    target: MixTarget,
}
fn output_track(
    key: &str,
    target: MixTarget,
    shown: FaderGain,
    color: u32,
    enabled: bool,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<gpui::Pixels>>));
    let painted = std::rc::Rc::clone(&bounds);
    let mut track = identified(key)
        .flex_1()
        .min_w(px(80.))
        .h(px(32.))
        .relative()
        .focusable()
        .tab_stop(enabled)
        .tab_index(0)
        .child(
            div()
                .absolute()
                .left(px(CAP / 2.))
                .right(px(CAP / 2.))
                .top(px(14.))
                .h(px(4.))
                .rounded_full()
                .bg(rgb(LINE)),
        )
        .child(
            div().absolute().left_0().right(px(CAP)).top(px(8.)).h(px(CAP)).child(
                div()
                    .absolute()
                    .left(relative(fader_fraction(shown)))
                    .w(px(CAP))
                    .h(px(CAP))
                    .rounded_full()
                    .bg(rgb(color)),
            ),
        )
        .child(
            canvas(move |area, _, _| painted.set(Some(area)), |_, (), _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
        );
    if enabled {
        track = track
            .cursor_pointer()
            .on_drag(OutputDrag { target }, |_, _, _, cx| cx.new(|_| gpui::Empty))
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<OutputDrag>, window, cx| {
                    if event.drag(cx).target != target {
                        return;
                    }
                    if let Some(fader) = output_at_pixel(
                        f32::from(event.event.position.x),
                        f32::from(event.bounds.left()),
                        f32::from(event.bounds.size.width),
                    ) {
                        this.fader_window = Some(window.window_handle());
                        this.send_fader(MixerEdit::Output { target, fader }, cx);
                    }
                },
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if let Some(area) = bounds.get()
                        && let Some(fader) = output_at_pixel(
                            f32::from(event.position.x),
                            f32::from(area.left()),
                            f32::from(area.size.width),
                        )
                    {
                        this.fader_window = Some(window.window_handle());
                        this.send_fader(MixerEdit::Output { target, fader }, cx);
                    }
                }),
            )
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                let fader = match event.keystroke.key.as_str() {
                    "home" => Some(FaderGain::MIN),
                    "end" => Some(FaderGain::MAX),
                    "left" | "down" => {
                        FaderGain::from_half_decibel_steps(shown.half_decibel_steps() - 1).ok()
                    }
                    "right" | "up" => {
                        FaderGain::from_half_decibel_steps(shown.half_decibel_steps() + 1).ok()
                    }
                    _ => None,
                };
                if let Some(fader) = fader {
                    cx.stop_propagation();
                    this.send_fader(MixerEdit::Output { target, fader }, cx);
                }
            }));
    }
    track
}
fn output_at_pixel(x: f32, left: f32, width: f32) -> Option<FaderGain> {
    if !x.is_finite() || !left.is_finite() || !width.is_finite() || width <= CAP {
        return None;
    }
    fader_from_fraction((x - left - CAP / 2.) / (width - CAP))
}

/// The level names between the two faders of a strip, aligned with their tracks.
fn scale() -> Div {
    let mut marks = div().size_full().relative();
    for level in SCALE {
        let fraction = FaderGain::from_decibels(f32::from(level)).map_or(0., fader_fraction);
        marks = marks.child(
            div()
                .absolute()
                .top(relative(1. - fraction))
                .mt(px(-7.))
                .left_0()
                .w(px(SCALE_WIDTH))
                .whitespace_nowrap()
                .text_center()
                .text_size(px(10.))
                .line_height(px(14.))
                .text_color(rgb(FAINT))
                .child(level.to_string().replace('-', "−")),
        );
    }
    div()
        .w(px(SCALE_WIDTH))
        .flex_shrink_0()
        .h_full()
        .flex()
        .flex_col()
        .child(div().w_full().flex_1().min_h(px(FADER_MIN)).py(px(CAP / 2.)).child(marks))
        .child(div().h(px(LANE_FOOT)).flex_shrink_0())
}

#[allow(clippy::too_many_arguments)]
fn fader_track(
    key: &str,
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    generation: MixerGeneration,
    shown: FaderGain,
    color: u32,
    enabled: bool,
    cx: &mut Context<Shell>,
) -> Stateful<Div> {
    let bounds = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<gpui::Pixels>>));
    let painted_bounds = std::rc::Rc::clone(&bounds);
    let mut track = identified(key)
        .w(px(FADER_WIDTH))
        .flex_1()
        .min_h(px(FADER_MIN))
        .py(px(CAP / 2.))
        .rounded(px(RADIUS))
        .relative()
        .flex()
        .justify_center()
        .focusable()
        .tab_index(0)
        .tab_stop(enabled)
        .focus_visible(|style| style.bg(rgb(RAISED)))
        .child(fader_rail(fader_fraction(shown), color))
        .child(
            canvas(move |area, _, _| painted_bounds.set(Some(area)), |_, (), _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
        );
    if enabled {
        track = track
            .cursor_pointer()
            .on_drag(FaderDrag { source, target }, |_, _, _, cx| cx.new(|_| gpui::Empty))
            .on_drag_move(cx.listener(move |this, event: &DragMoveEvent<FaderDrag>, window, cx| {
                let drag = event.drag(cx);
                if drag.source != source || drag.target != target {
                    return;
                }
                if let Some(fader) = fader_at_pixel(
                    f32::from(event.event.position.y),
                    f32::from(event.bounds.top()),
                    f32::from(event.bounds.size.height),
                ) {
                    this.fader_window = Some(window.window_handle());
                    this.send_fader(FaderEdit { source, target, fader }, cx);
                }
            }))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                if let Some(command) =
                    route_key_command(source, target, route, generation, &event.keystroke.key)
                {
                    cx.stop_propagation();
                    this.send(command, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if let Some(area) = bounds.get()
                        && let Some(fader) = fader_at_pixel(
                            f32::from(event.position.y),
                            f32::from(area.top()),
                            f32::from(area.size.height),
                        )
                    {
                        this.fader_window = Some(window.window_handle());
                        this.send_fader(FaderEdit { source, target, fader }, cx);
                    }
                }),
            );
    }
    track
}

/// The cap center travels between the two inset ends of this rail.
fn fader_rail(position: f32, color: u32) -> Div {
    div()
        .relative()
        .w_full()
        .h_full()
        .child(
            div()
                .absolute()
                .left(px(FADER_WIDTH / 2. - 2.))
                .w(px(4.))
                .h_full()
                .rounded_full()
                .bg(rgb(LINE)),
        )
        .child(
            div()
                .absolute()
                .left(px(FADER_WIDTH / 2. - 2.))
                .bottom_0()
                .w(px(4.))
                .h(relative(position))
                .rounded_full()
                .bg(rgb(color)),
        )
        .child(
            div()
                .absolute()
                .top(relative(1. - fader_fraction(FaderGain::UNITY)))
                .left(px(FADER_WIDTH / 2. - 8.))
                .w(px(16.))
                .h(px(1.))
                .bg(rgb(FAINT)),
        )
        .child(
            div()
                .absolute()
                .top(relative(1. - position))
                .mt(px(-CAP / 2.))
                .left(px(4.))
                .w(px(FADER_WIDTH - 8.))
                .h(px(CAP))
                .rounded(px(5.))
                .bg(rgb(INK))
                .flex()
                .items_center()
                .justify_center()
                .child(div().w(px(14.)).h(px(2.)).rounded_full().bg(rgb(PANEL))),
        )
}

/// Maps a pointer position on a vertical track to the nearest fader step.
fn fader_at_pixel(y: f32, top: f32, height: f32) -> Option<FaderGain> {
    if !y.is_finite() || !top.is_finite() || !height.is_finite() || height <= CAP {
        return None;
    }
    fader_from_fraction(1. - (y - top - CAP / 2.) / (height - CAP))
}

#[allow(clippy::cast_possible_truncation)]
fn fader_from_fraction(fraction: f32) -> Option<FaderGain> {
    if !fraction.is_finite() {
        return None;
    }
    let minimum = FaderGain::MIN.half_decibel_steps();
    let span = FaderGain::MAX.half_decibel_steps() - minimum;
    // The clamped, rounded value is bounded by the exact fader interval.
    let steps = (fraction.clamp(0., 1.) * f32::from(span)).round() as i16 + minimum;
    FaderGain::from_half_decibel_steps(steps).ok()
}

pub(super) fn fader_fraction(fader: FaderGain) -> f32 {
    f32::from(fader.half_decibel_steps() - FaderGain::MIN.half_decibel_steps())
        / f32::from(FaderGain::MAX.half_decibel_steps() - FaderGain::MIN.half_decibel_steps())
}

pub(super) fn route_key_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    generation: MixerGeneration,
    key: &str,
) -> Option<Command> {
    match key {
        "up" | "right" => route_step_command(source, target, route, generation, 1),
        "down" | "left" => route_step_command(source, target, route, generation, -1),
        "home" => route_position_command(source, target, route, generation, 0.),
        "end" => route_position_command(source, target, route, generation, 1.),
        "enter" => route_position_command(
            source,
            target,
            route,
            generation,
            fader_fraction(FaderGain::UNITY),
        ),
        _ => None,
    }
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn route_position_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    expected_generation: MixerGeneration,
    fraction: f32,
) -> Option<Command> {
    let fader = fader_from_fraction(fraction)?;
    if fader == route.fader() {
        return None;
    }
    Some(Command::SetMixerRoute {
        expected_generation,
        source,
        target,
        route: MixRoute::new(route.enabled(), fader),
    })
}

pub(super) fn route_toggle_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    expected_generation: MixerGeneration,
) -> Command {
    Command::SetMixerRoute {
        expected_generation,
        source,
        target,
        route: MixRoute::new(!route.enabled(), route.fader()),
    }
}

pub(super) fn route_step_command(
    source: SourceId,
    target: MixTarget,
    route: MixRoute,
    expected_generation: MixerGeneration,
    delta: i16,
) -> Option<Command> {
    let steps = route.fader().half_decibel_steps().checked_add(delta)?;
    let fader = FaderGain::from_half_decibel_steps(steps).ok()?;
    Some(Command::SetMixerRoute {
        expected_generation,
        source,
        target,
        route: MixRoute::new(route.enabled(), fader),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_output_pointer_positions_round_trip_every_step_and_reject_invalid_bounds() {
        let left = 24.0;
        let width = 160.0;
        for step in -120..=24 {
            let fader = FaderGain::from_half_decibel_steps(step).expect("step");
            let x = left + CAP / 2.0 + fader_fraction(fader) * (width - CAP);
            assert_eq!(output_at_pixel(x, left, width), Some(fader));
        }
        assert_eq!(output_at_pixel(-100.0, left, width), Some(FaderGain::MIN));
        assert_eq!(output_at_pixel(1000.0, left, width), Some(FaderGain::MAX));
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(output_at_pixel(invalid, left, width).is_none());
        }
        assert!(output_at_pixel(24.0, left, CAP).is_none());
    }

    #[test]
    fn pointer_geometry_covers_every_step_and_clamps_drag_beyond_the_track() {
        for step in -120..=24 {
            let fader = FaderGain::from_half_decibel_steps(step).expect("step");
            let y = 100. + CAP / 2. + (1. - fader_fraction(fader)) * (260. - CAP);
            assert_eq!(fader_at_pixel(y, 100., 260.), Some(fader));
        }
        assert_eq!(fader_at_pixel(-100., 100., 260.), Some(FaderGain::MAX));
        assert_eq!(fader_at_pixel(1000., 100., 260.), Some(FaderGain::MIN));
        for (y, top, height) in [
            (f32::NAN, 0., 260.),
            (1., f32::INFINITY, 260.),
            (1., 0., f32::NAN),
            (1., 0., CAP),
            (1., 0., -1.),
        ] {
            assert!(fader_at_pixel(y, top, height).is_none());
        }
    }

    #[test]
    fn inputs_show_their_role_or_named_kind_and_unnamed_inputs_differ() {
        let profile = librewave_core::MixerProfile::first_run(&[]);
        let glyphs: Vec<_> = profile.sources.iter().map(input_glyph).collect();
        assert_eq!(
            glyphs,
            ["mic", "system", "music-notes", "globe-simple", "chats-circle", "game-controller"]
        );
        let mut generic = profile.clone();
        for name in ["Input A", "Input B"] {
            generic = generic
                .with_input(librewave_core::InputName::try_new(name).expect("name"))
                .expect("add");
        }
        let count = generic.sources.len();
        assert_ne!(
            input_glyph(&generic.sources[count - 1]),
            input_glyph(&generic.sources[count - 2])
        );
    }

    #[test]
    fn dropping_a_channel_moves_it_once_and_ignores_its_own_place() {
        let snapshot = Snapshot::empty();
        let sources = &snapshot.mixer.profile.sources;
        let first = sources[0].controls.source();
        let last = sources.last().expect("built-in channels").controls.source();
        assert_eq!(
            channel_move(&snapshot, first, 1),
            Some(Command::MoveMixerInput {
                expected_generation: snapshot.mixer.generation(),
                source: first,
                position: 1,
            })
        );
        assert!(channel_move(&snapshot, first, 0).is_none());
        // A move past the end stops at the last place.
        assert!(channel_move(&snapshot, last, 99).is_none());
        assert!(channel_move(&snapshot, SourceId::new(999), 0).is_none());
    }

    #[test]
    fn inputs_keep_room_for_a_usable_fader_and_the_scale_names_real_levels() {
        // Section label, strip padding, heading, meter, mix names, and their gaps.
        let chrome =
            16. + SPACE_MD + 2. * SPACE_LG + 40. + SPACE_XL + 9. + SPACE_XL + 28. + SPACE_MD;
        assert!(chrome + FADER_MIN + LANE_FOOT <= INPUTS_MIN);
        for level in SCALE {
            assert!(FaderGain::from_decibels(f32::from(level)).is_ok());
        }
        assert!(mixer_min_width() > 2. * STRIP_WIDTH);
    }
}

pub(super) fn input_layout_available(runtime: librewave_core::MixerRuntimeState) -> bool {
    matches!(
        runtime,
        librewave_core::MixerRuntimeState::Active
            | librewave_core::MixerRuntimeState::Inactive {
                reason: librewave_core::MixerInactiveReason::AudioHostEngineNotConnected
            }
    )
}

#[cfg(test)]
mod input_layout_tests {
    use super::*;
    use librewave_core::{MixerInactiveReason, MixerRuntimeState};
    #[test]
    fn live_edits_are_available_only_when_ready_or_cleanly_stopped() {
        assert!(input_layout_available(MixerRuntimeState::Active));
        assert!(input_layout_available(MixerRuntimeState::Inactive {
            reason: MixerInactiveReason::AudioHostEngineNotConnected
        }));
        for reason in [MixerInactiveReason::Starting, MixerInactiveReason::CleanupPending] {
            assert!(!input_layout_available(MixerRuntimeState::Inactive { reason }));
        }
    }
}
