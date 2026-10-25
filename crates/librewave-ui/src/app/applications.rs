//! The channel pop-up: channel name, order, and application rules.
//!
//! Application selection stays on the versioned daemon interface. The list of
//! installed applications comes from desktop entries and changes nothing.
use super::{
    Shell,
    controls::{Tone, activate, button_face, caption, icon, icon_button, identified, label, tip},
    text_field::{Submitted, TextField},
    theme::{
        BODY, BODY_LINE, CAPTION, CAPTION_LINE, DANGER, FAINT, GROUP, HEADING, HEADING_LINE,
        ICON_SMALL, INK, LINE, MONITOR, MUTED, RADIUS, RADIUS_CARD, RAISED, SPACE_LG, SPACE_MD,
        SPACE_SM, SPACE_XS, WARNING,
    },
};
use gpui::{
    Anchor, AnyElement, BoxShadow, Context, Div, FontWeight, Stateful, anchored, deferred, div,
    prelude::*, px, rgb,
};
use librewave_core::{
    ApplicationId, ApplicationMovement, ApplicationRoute, ApplicationSnapshot, Command, InputName,
    Snapshot, SourceId, SourceRole,
};
use librewave_ipc::Response;
use librewave_platform_linux::desktop_entries::{self, InstalledApplication};
use librewave_platform_linux::ipc::{Client, ClientError, configured_socket_path};
use std::time::{Duration, Instant};

const MENU_WIDTH: f32 = 380.;

/// Installed applications, read once when a channel pop-up first opens.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum Installed {
    #[default]
    NotRead,
    Reading,
    Ready(Vec<InstalledApplication>),
}

impl Installed {
    fn list(&self) -> &[InstalledApplication] {
        match self {
            Self::Ready(applications) => applications,
            Self::NotRead | Self::Reading => &[],
        }
    }

    /// The installed entry for an application identity: the desktop ID, or
    /// the program that the entry starts.
    fn find(&self, application: ApplicationId) -> Option<&InstalledApplication> {
        let list = self.list();
        match application {
            ApplicationId::Desktop(id) => list
                .iter()
                .find(|app| app.id == id.as_str())
                .or_else(|| list.iter().find(|app| app.id.eq_ignore_ascii_case(id.as_str()))),
            ApplicationId::Executable(name) => {
                list.iter().find(|app| app.executable.as_deref() == Some(name.as_str()))
            }
        }
    }
}
const LIST_HEIGHT: f32 = 300.;
const NOTE_HEIGHT: f32 = CAPTION_LINE + SPACE_MD;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum RuleKind {
    Desktop,
    Executable,
}

impl RuleKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Desktop => "App ID",
            Self::Executable => "Executable",
        }
    }

    const fn glyph(self) -> &'static str {
        match self {
            Self::Desktop => "app-window",
            Self::Executable => "terminal-window",
        }
    }
}

fn kind_of(application: ApplicationId) -> (RuleKind, InputName) {
    match application {
        ApplicationId::Desktop(name) => (RuleKind::Desktop, name),
        ApplicationId::Executable(name) => (RuleKind::Executable, name),
    }
}

/// One selectable row in the application list.
struct Choice {
    key: String,
    application: ApplicationId,
    name: String,
    detail: String,
    icon: Option<std::path::PathBuf>,
    enabled: bool,
    hint: &'static str,
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
            let field = cx.new(|cx| TextField::new(&name, cx));
            cx.subscribe(&field, |this, _, _: &Submitted, cx| this.save_channel_text(false, cx))
                .detach();
            self.channel_name = Some(field);
        }
        self.set_assignment_text("", cx);
        self.load_installed_applications(cx);
    }

    /// Sets the custom entry, creating it when the pop-up opens. It also
    /// filters the list, so the pop-up redraws whenever its text changes.
    fn set_assignment_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(field) = &self.assignment_name {
            field.update(cx, |field, cx| field.set_value(text, cx));
            return;
        }
        let field = cx.new(|cx| {
            TextField::new(text, cx).with_placeholder("Search, or type an app ID or executable")
        });
        cx.observe(&field, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&field, |this, _, _: &Submitted, cx| this.save_channel_text(true, cx))
            .detach();
        self.assignment_name = Some(field);
    }

    fn load_installed_applications(&mut self, cx: &mut Context<Self>) {
        if self.installed_applications != Installed::NotRead {
            return;
        }
        self.installed_applications = Installed::Reading;
        cx.spawn(async move |this, cx| {
            let applications = cx
                .background_executor()
                .spawn(async move { desktop_entries::installed_applications() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.installed_applications = Installed::Ready(applications);
                cx.notify();
            });
        })
        .detach();
    }

    fn save_channel_text(&mut self, assignment: bool, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else { return };
        let Some(source) = self.application_input else { return };
        let field = if assignment { &self.assignment_name } else { &self.channel_name };
        let Some(field) = field else { return };
        let name = match InputName::try_new(field.read(cx).value().trim()) {
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
                application: match self.rule_kind(name.as_str()) {
                    RuleKind::Desktop => ApplicationId::Desktop(name),
                    RuleKind::Executable => ApplicationId::Executable(name),
                },
                source: Some(source),
            }
        } else {
            Command::RenameMixerInput { expected_generation, source, name }
        };
        self.send(command, cx);
    }

    /// Decides whether typed text names an app ID or an executable.
    ///
    /// A file chosen with the picker keeps the kind it was chosen as. Text
    /// that names an installed or playing application takes that kind. Other
    /// text with a dot reads as a reverse-domain app ID, such as
    /// `com.spotify.Client`, and anything else as an executable name.
    pub(super) fn rule_kind(&self, text: &str) -> RuleKind {
        let text = text.trim();
        if let Some((picked, kind)) = &self.picked_rule
            && picked == text
        {
            return *kind;
        }
        let installed = self.installed_applications.list();
        if installed.iter().any(|app| app.id == text) {
            return RuleKind::Desktop;
        }
        if installed.iter().any(|app| app.executable.as_deref() == Some(text)) {
            return RuleKind::Executable;
        }
        if let Some(ApplicationSnapshot::Ready { applications, .. }) = &self.applications
            && let Some(app) = applications.iter().find(|app| kind_of(app.id).1.as_str() == text)
        {
            return kind_of(app.id).0;
        }
        if text.contains('.') { RuleKind::Desktop } else { RuleKind::Executable }
    }

    /// Fills the custom entry from a chosen file. A desktop entry gives its
    /// desktop ID; any other file gives its name as an executable.
    fn choose_application_file(cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose an application or its desktop entry".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = selection.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            let (kind, text) = match desktop_entries::desktop_id(path) {
                                Some(id) => (RuleKind::Desktop, id),
                                None => (
                                    RuleKind::Executable,
                                    path.file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                        .unwrap_or_default(),
                                ),
                            };
                            this.set_assignment_text(&text, cx);
                            this.picked_rule = Some((text, kind));
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => this.action_error = Some(error.to_string()),
                    Err(error) => this.action_error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
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
}

impl Shell {
    /// The pop-up for one channel, anchored under its strip heading.
    pub(super) fn channel_menu(
        &self,
        snapshot: &Snapshot,
        source: SourceId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menu = div()
            .id("channel-menu")
            .debug_selector(|| "channel-menu".to_owned())
            .occlude()
            .w(px(MENU_WIDTH))
            .rounded(px(RADIUS_CARD))
            // Lighter than the fields, so they read as inset entries.
            .bg(rgb(GROUP))
            .shadow(vec![
                BoxShadow::new(px(0.), px(16.), gpui::black().opacity(0.55)).blur_radius(px(48.)),
            ])
            .flex()
            .flex_col()
            .child(self.menu_heading(snapshot, source, cx))
            .child(div().h(px(1.)).bg(rgb(LINE)))
            .child(self.application_list(snapshot, source, cx))
            .child(div().h(px(1.)).bg(rgb(LINE)))
            .child(self.custom_entry(cx))
            // The note keeps its line when empty, so the menu never changes height.
            .child(div().h(px(NOTE_HEIGHT)).flex().flex_col().children(self.routing_note()))
            .children(self.removal_footer(snapshot, source, cx));
        deferred(
            anchored().anchor(Anchor::TopLeft).snap_to_window_with_margin(px(SPACE_MD)).child(menu),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn menu_heading(&self, snapshot: &Snapshot, source: SourceId, cx: &mut Context<Self>) -> Div {
        let input = snapshot.mixer.profile.source(source);
        let title = match &self.channel_name {
            Some(field) => div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(SPACE_XS))
                .child(field.clone())
                .child(icon_button(
                    "rename-channel",
                    "check",
                    "Save the channel name",
                    Tone::Secondary,
                    INK,
                    !self.busy(),
                    cx,
                    |this, _, cx| this.save_channel_text(false, cx),
                )),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(HEADING))
                .line_height(px(HEADING_LINE))
                .font_weight(FontWeight::SEMIBOLD)
                .child(input.map_or_else(String::new, |input| input.name.as_str().to_owned())),
        };
        div()
            .p(px(SPACE_MD))
            .pl(px(SPACE_LG))
            .flex()
            .items_center()
            .gap(px(SPACE_SM))
            .child(title)
            .child(icon_button(
                "close-channel",
                "x",
                "Close",
                Tone::Quiet,
                MUTED,
                true,
                cx,
                |this, _, cx| this.close_channel(cx),
            ))
    }

    /// Rules for this channel, then playing applications, then installed ones.
    fn application_list(
        &self,
        snapshot: &Snapshot,
        source: SourceId,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let query = self
            .assignment_name
            .as_ref()
            .map(|field| field.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();
        let matches = |name: &str, detail: &str| {
            query.is_empty()
                || name.to_lowercase().contains(&query)
                || detail.to_lowercase().contains(&query)
        };
        let rules = &snapshot.mixer.profile.assignments;
        let channel = |id: SourceId| {
            snapshot.mixer.profile.source(id).map_or("System", |s| s.name.as_str()).to_owned()
        };
        let assigned_elsewhere = |application: ApplicationId| {
            rules
                .iter()
                .find(|rule| rule.application == application && rule.source != source)
                .map(|rule| format!("In {}", channel(rule.source)))
        };
        let mut list = div()
            .id("application-list")
            .h(px(LIST_HEIGHT))
            .overflow_y_scroll()
            .p(px(SPACE_SM))
            .flex()
            .flex_col();

        let mut empty = true;
        let here: Vec<Choice> = rules
            .iter()
            .filter(|rule| rule.source == source)
            .enumerate()
            .filter_map(|(index, rule)| {
                let (kind, id) = kind_of(rule.application);
                let installed = self.installed_applications.find(rule.application);
                let name = installed.map_or_else(|| id.as_str().to_owned(), |app| app.name.clone());
                let detail = format!("{} · {}", kind.label(), id.as_str());
                matches(&name, &detail).then(|| Choice {
                    key: format!("app-assigned-{index}"),
                    application: rule.application,
                    name,
                    detail,
                    icon: installed.and_then(|app| app.icon.clone()),
                    enabled: !self.busy(),
                    hint: "Forget this assignment",
                })
            })
            .collect();
        empty &= here.is_empty();
        list = Self::list_section(list, "Assigned here", here, source, snapshot, cx);

        let active = self.active_choices(snapshot, source, &matches, &assigned_elsewhere);
        empty &= active.is_empty();
        list = Self::list_section(list, "Active", active, source, snapshot, cx);

        let installed: Vec<Choice> = self
            .installed_applications
            .list()
            .iter()
            .enumerate()
            .filter_map(|(index, app): (usize, &InstalledApplication)| {
                let id = InputName::try_new(&app.id).ok()?;
                let application = ApplicationId::Desktop(id);
                if rules.iter().any(|r| r.application == application && r.source == source)
                    || self.is_playing(application, app)
                {
                    return None;
                }
                let detail = assigned_elsewhere(application).unwrap_or_else(|| app.id.clone());
                matches(&app.name, &app.id).then(|| Choice {
                    key: format!("app-installed-{index}"),
                    application,
                    name: app.name.clone(),
                    detail,
                    icon: app.icon.clone(),
                    enabled: !self.busy(),
                    hint: "Assign to this channel when it plays",
                })
            })
            .collect();
        let loading = self.installed_applications == Installed::Reading;
        empty &= installed.is_empty();
        list = Self::list_section(list, "All applications", installed, source, snapshot, cx);
        if loading {
            list = list.child(note("Reading installed applications…"));
        } else if empty {
            list = list.child(note("No applications match."));
        }
        list
    }

    /// Whether an installed entry is already listed as a playing application.
    fn is_playing(&self, application: ApplicationId, entry: &InstalledApplication) -> bool {
        let Some(ApplicationSnapshot::Ready { applications, .. }) = &self.applications else {
            return false;
        };
        applications.iter().any(|playing| {
            playing.id == application
                || matches!(playing.id, ApplicationId::Executable(name)
                    if entry.executable.as_deref() == Some(name.as_str()))
        })
    }

    /// Playing applications that are not assigned to this channel yet.
    fn active_choices(
        &self,
        snapshot: &Snapshot,
        source: SourceId,
        matches: &dyn Fn(&str, &str) -> bool,
        assigned_elsewhere: &dyn Fn(ApplicationId) -> Option<String>,
    ) -> Vec<Choice> {
        let rules = &snapshot.mixer.profile.assignments;
        match &self.applications {
            Some(ApplicationSnapshot::Ready { applications, movement, route, .. }) => {
                let busy = matches!(
                    route,
                    Some(ApplicationRoute::Pending { .. } | ApplicationRoute::Restoring { .. })
                );
                let fresh = self
                    .applications_received
                    .is_some_and(|t| t.elapsed() <= Duration::from_millis(1500));
                let movable = *movement == ApplicationMovement::Allowed && !busy && fresh;
                applications
                    .iter()
                    .enumerate()
                    .filter(|(_, app)| {
                        !rules.iter().any(|r| r.application == app.id && r.source == source)
                    })
                    .filter_map(|(index, app)| {
                        let streams = if app.streams == 1 {
                            "1 stream".to_owned()
                        } else {
                            format!("{} streams", app.streams)
                        };
                        let detail = assigned_elsewhere(app.id)
                            .map_or(streams.clone(), |place| format!("{streams} · {place}"));
                        let name = app.name.as_str().to_owned();
                        matches(&name, &detail).then(|| Choice {
                            key: format!("app-active-{index}"),
                            application: app.id,
                            name,
                            detail,
                            icon: self
                                .installed_applications
                                .find(app.id)
                                .and_then(|installed| installed.icon.clone()),
                            enabled: !self.busy() && movable && !app.move_blocked,
                            hint: if app.move_blocked {
                                "This application does not allow its audio to move"
                            } else {
                                "Assign to this channel"
                            },
                        })
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn list_section(
        list: Stateful<Div>,
        name: &str,
        choices: Vec<Choice>,
        source: SourceId,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if choices.is_empty() {
            return list;
        }
        let generation = snapshot.mixer.generation();
        let mut list = list.child(label(name).px(px(SPACE_SM)).pt(px(SPACE_SM)).pb(px(SPACE_XS)));
        for choice in choices {
            let assigned = snapshot
                .mixer
                .profile
                .assignments
                .iter()
                .any(|r| r.application == choice.application && r.source == source);
            let application = choice.application;
            let (kind, _) = kind_of(application);
            let row = identified(&choice.key)
                .w_full()
                .min_h(px(44.))
                .px(px(SPACE_SM))
                .py(px(6.))
                .rounded(px(RADIUS))
                .border_1()
                .border_color(gpui::rgba(0))
                .flex()
                .items_center()
                .gap(px(SPACE_MD))
                .hover(|style| style.bg(rgb(RAISED)))
                .child(application_icon(choice.icon, kind, assigned))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(BODY))
                                .line_height(px(BODY_LINE))
                                .child(choice.name),
                        )
                        .child(caption(choice.detail).text_color(rgb(FAINT)).truncate()),
                )
                .child(if assigned {
                    icon("x", MUTED, ICON_SMALL)
                } else {
                    icon("plus", FAINT, ICON_SMALL)
                })
                .tooltip(tip(choice.hint));
            list = list.child(activate(row, choice.enabled, cx, move |this, _, cx| {
                this.send(
                    Command::SetApplicationAssignment {
                        expected_generation: generation,
                        application,
                        source: (!assigned).then_some(source),
                    },
                    cx,
                );
            }));
        }
        list
    }

    /// A custom rule: its kind, its identity, and a file picker that fills it.
    fn custom_entry(&self, cx: &mut Context<Self>) -> Div {
        let text = self
            .assignment_name
            .as_ref()
            .map(|field| field.read(cx).value().trim().to_owned())
            .unwrap_or_default();
        let kind = self.rule_kind(&text);
        let mut row = div().p(px(SPACE_MD)).flex().items_center().gap(px(SPACE_XS)).child(
            identified("rule-kind")
                .size(px(32.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .child(icon(kind.glyph(), if text.is_empty() { FAINT } else { MUTED }, ICON_SMALL))
                .tooltip(tip(match kind {
                    RuleKind::Desktop => "Typed text is saved as an app ID",
                    RuleKind::Executable => "Typed text is saved as an executable name",
                })),
        );
        if let Some(field) = &self.assignment_name {
            row = row.child(div().flex_1().min_w_0().child(field.clone()));
        }
        row.child(icon_button(
            "choose-application",
            "folder-open",
            "Choose an application or desktop entry",
            Tone::Quiet,
            MUTED,
            !self.busy(),
            cx,
            |_, _, cx| Shell::choose_application_file(cx),
        ))
        .child(icon_button(
            "save-rule",
            "plus",
            "Add this rule to the channel",
            Tone::Secondary,
            INK,
            !self.busy()
                && self
                    .assignment_name
                    .as_ref()
                    .is_some_and(|field| !field.read(cx).value().trim().is_empty()),
            cx,
            |this, _, cx| this.save_channel_text(true, cx),
        ))
    }

    /// Removes a software input after a confirmation. Built-in inputs have none.
    fn removal_footer(
        &self,
        snapshot: &Snapshot,
        source: SourceId,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let input = snapshot.mixer.profile.source(source)?;
        if input.role != SourceRole::Software {
            return None;
        }
        let name = input.name.clone();
        let generation = snapshot.mixer.generation();
        let enabled = !self.busy() && super::mixer::input_layout_available(snapshot.mixer.runtime);
        let button = activate(
            button_face("remove-input", Tone::Quiet)
                .px(px(SPACE_SM))
                .text_color(rgb(DANGER))
                .child(icon("trash", DANGER, ICON_SMALL))
                .child("Remove input"),
            enabled,
            cx,
            move |this, window, cx| {
                this.close_channel(cx);
                this.ask(
                    format!("Remove {name}?"),
                    "Its levels are deleted. Applications assigned to it move to System.",
                    "Remove",
                    Command::RemoveMixerInput { expected_generation: generation, source },
                    window,
                    cx,
                );
            },
        );
        Some(
            div().border_t_1().border_color(rgb(LINE)).p(px(SPACE_SM)).flex().items_center().child(
                if enabled {
                    button
                } else {
                    button.tooltip(tip("Wait for the mixer to be ready before removing an input"))
                },
            ),
        )
    }

    /// Routing progress and conditions that limit assignment.
    fn routing_note(&self) -> Option<Div> {
        let text = match &self.applications {
            Some(ApplicationSnapshot::Ready { movement, route, unidentified_streams, .. }) => {
                if *movement != ApplicationMovement::Allowed {
                    Some((WARNING, "WirePlumber does not allow streams to move.".to_owned()))
                } else if let Some(route) = route {
                    Some(match route {
                        ApplicationRoute::Pending { .. } => {
                            (MUTED, "Moving the application audio…".to_owned())
                        }
                        ApplicationRoute::Verified { .. } => {
                            (MONITOR, "The application audio is on this channel.".to_owned())
                        }
                        ApplicationRoute::Restoring { .. } => {
                            (MUTED, "Restoring the previous audio targets…".to_owned())
                        }
                        ApplicationRoute::Failed { reason, .. } => (WARNING, reason.clone()),
                    })
                } else if *unidentified_streams > 0 {
                    Some((
                        FAINT,
                        if *unidentified_streams == 1 {
                            "1 playing stream does not name its application.".to_owned()
                        } else {
                            format!(
                                "{unidentified_streams} playing streams do not name their \
                                 application."
                            )
                        },
                    ))
                } else {
                    None
                }
            }
            _ if !self.audio_active() => {
                Some((FAINT, "Start audio to see playing applications.".to_owned()))
            }
            _ => self.application_error.clone().map(|error| (WARNING, error)),
        };
        text.map(|(color, text)| {
            div()
                .px(px(SPACE_LG))
                .truncate()
                .text_size(px(CAPTION))
                .line_height(px(CAPTION_LINE))
                .text_color(rgb(color))
                .child(text)
        })
    }
}

/// The application's own icon, or a generic glyph when it has none.
fn application_icon(path: Option<std::path::PathBuf>, kind: RuleKind, assigned: bool) -> Div {
    let tile = div().size(px(28.)).flex_shrink_0().flex().items_center().justify_center();
    match path {
        Some(path) => tile.child(gpui::img(path).size(px(24.))),
        None => tile.rounded(px(6.)).bg(rgb(RAISED)).child(icon(
            kind.glyph(),
            if assigned { MONITOR } else { MUTED },
            ICON_SMALL,
        )),
    }
}

fn note(text: &str) -> Div {
    div()
        .px(px(SPACE_SM))
        .py(px(SPACE_SM))
        .text_size(px(CAPTION))
        .line_height(px(CAPTION_LINE))
        .text_color(rgb(FAINT))
        .child(text.to_owned())
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
                            field,
                            Some(0..5),
                            "Unsaved",
                            window,
                            cx,
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
    fn channel_menu_floats_over_the_mixer_and_an_outside_click_only_closes_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| shell_with(Snapshot::empty()));
        let before = cx.debug_bounds("fader-2-monitor").expect("system fader");
        let opener = cx.debug_bounds("edit-input-2").expect("edit button");
        cx.simulate_click(opener.center(), gpui::Modifiers::none());
        let menu = cx.debug_bounds("channel-menu").expect("open menu");
        assert_eq!(cx.debug_bounds("fader-2-monitor"), Some(before), "the strip moved");
        // The fixture application is listed and the filter can hide it.
        assert!(cx.debug_bounds("app-installed-0").is_some());
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let field = view.assignment_name.clone().expect("custom entry");
                field.update(cx, |field, cx| {
                    gpui::EntityInputHandler::replace_text_in_range(
                        field,
                        None,
                        "no such app",
                        window,
                        cx,
                    );
                });
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("app-installed-0").is_none());
        let filtered = cx.debug_bounds("channel-menu").expect("filtered menu");
        assert_eq!(filtered.size, menu.size, "filtering changed the menu size");
        let outside = gpui::point(menu.left() - px(40.), menu.bottom() + px(40.));
        cx.simulate_click(outside, gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert!(view.application_input.is_none());
            assert!(!view.pending);
            assert!(view.in_flight_fader.is_none() && view.queued_fader.is_none());
        });
        assert!(cx.debug_bounds("channel-menu").is_none());
    }

    #[cfg(feature = "gpui-tests")]
    #[gpui::test]
    fn removing_an_input_closes_the_menu_and_asks_first(cx: &mut gpui::TestAppContext) {
        let mut snapshot = Snapshot::empty();
        snapshot.mixer.profile = snapshot
            .mixer
            .profile
            .with_input(InputName::try_new("Music").expect("valid name"))
            .expect("available input");
        let (view, cx) = cx.add_window_view(|_, _| shell_with(snapshot));
        let system = cx.debug_bounds("edit-input-2").expect("system edit button");
        cx.simulate_click(system.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("channel-menu").is_some());
        assert!(cx.debug_bounds("remove-input").is_none(), "System cannot be removed");
        cx.update(|_, cx| view.update(cx, Shell::close_channel));
        let music = cx.debug_bounds("edit-input-3").expect("music edit button");
        cx.simulate_click(music.center(), gpui::Modifiers::none());
        let remove = cx.debug_bounds("remove-input").expect("remove button");
        cx.simulate_click(remove.center(), gpui::Modifiers::none());
        view.read_with(cx, |view, _| {
            assert!(view.application_input.is_none());
            assert!(!view.pending, "asking sends nothing");
            assert!(matches!(
                view.confirmation.as_ref().map(|c| c.command),
                Some(Command::RemoveMixerInput { source, .. }) if source == SourceId::new(3)
            ));
        });
        assert!(cx.debug_bounds("channel-menu").is_none());
        assert!(cx.debug_bounds("confirmation").is_some());
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
    fn typed_identities_choose_their_rule_kind_without_a_toggle() {
        let mut shell = shell_with(Snapshot::empty());
        // The fixture installs `org.test.Player`, which starts `player`.
        assert!(shell.rule_kind("org.test.Player") == RuleKind::Desktop);
        assert!(shell.rule_kind(" player ") == RuleKind::Executable);
        assert!(shell.rule_kind("com.example.Unknown") == RuleKind::Desktop);
        assert!(shell.rule_kind("unknown-tool") == RuleKind::Executable);
        // A file chosen as an executable stays one even with a dot in its name.
        shell.picked_rule = Some(("tool.bin".to_owned(), RuleKind::Executable));
        assert!(shell.rule_kind("tool.bin") == RuleKind::Executable);
        assert!(shell.rule_kind("tool.bin2") == RuleKind::Desktop);
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
