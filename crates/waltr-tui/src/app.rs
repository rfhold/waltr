//! App state and event handling for the terminal UI.

use std::time::{Duration, Instant};

use waltr_client::{ClientCommand, ClientSnapshot};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::{
    event::AppEvent,
    input::InputEvent,
    keys::{KeyCommand, LeaderCommand, key_command},
    screens::{
        ConfigTab, McpSection, ProviderSection, RuntimeDefaultsSection, SectionAction,
        SessionsSection,
    },
    ui,
    ui::Notice,
};

pub use crate::screens::{
    McpForm, McpFormMode, ModelForm, ModelFormMode, ProviderForm, ProviderFormMode,
};

const EXIT_ARM_DURATION: Duration = Duration::from_millis(1_500);
const NOTICE_DURATION: Duration = Duration::from_millis(1_500);

/// Result of applying an input event to the app.
#[derive(Clone, Debug, PartialEq)]
pub enum AppAction {
    /// Continue running the TUI.
    Continue,
    /// Exit the TUI.
    Exit,
    /// Dispatch a shared client command.
    ClientCommand(ClientCommand),
    /// Dispatch shared client commands in order.
    ClientCommands(Vec<ClientCommand>),
}

/// The currently armed close confirmation, if any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseIntent {
    /// Ctrl-C has been pressed once and is awaiting confirmation.
    CtrlC,
    /// Escape has been pressed once and is awaiting confirmation.
    Escape,
}

/// Active top-level TUI screen.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AppScreen {
    /// Waiting for the shared client to observe server readiness.
    #[default]
    Waiting,
    /// Ready landing screen with command palette.
    Landing,
    /// Read-only configuration view.
    Config,
    /// Server-side session list/detail view.
    Sessions,
}

impl CloseIntent {
    const fn notice(self) -> &'static str {
        match self {
            Self::CtrlC => "press Ctrl-C again to exit",
            Self::Escape => "press Esc again to exit",
        }
    }
}

/// Mutable state for the Waltr TUI.
#[derive(Debug, Default)]
pub struct App {
    armed_close: Option<ArmedClose>,
    active_config_tab: ConfigTab,
    provider_section: ProviderSection,
    mcp_section: McpSection,
    runtime_defaults_section: RuntimeDefaultsSection,
    sessions_section: SessionsSection,
    leader_armed: bool,
    notice: Option<TransientNotice>,
    screen: AppScreen,
    client_snapshot: Option<ClientSnapshot>,
}

impl App {
    /// Create a fresh app instance.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            armed_close: None,
            active_config_tab: ConfigTab::Providers,
            provider_section: ProviderSection::new(),
            mcp_section: McpSection::new(),
            runtime_defaults_section: RuntimeDefaultsSection::new(),
            sessions_section: SessionsSection::new(),
            leader_armed: false,
            notice: None,
            screen: AppScreen::Waiting,
            client_snapshot: None,
        }
    }

    /// Render the app into a Ratatui frame.
    pub fn draw(&self, frame: &mut Frame<'_>) {
        ui::draw(frame, self);
    }

    /// Handle one normalized input event.
    pub fn handle_input(&mut self, input: InputEvent, now: Instant) -> AppAction {
        self.handle_event(&AppEvent::Input(input), now)
    }

    /// Handle one app-level event.
    pub fn handle_event(&mut self, event: &AppEvent, now: Instant) -> AppAction {
        let now = event.timestamp(now);
        self.update(now);

        match event {
            AppEvent::Tick(_) | AppEvent::Stream(_) => AppAction::Continue,
            AppEvent::Client(snapshot) => {
                AppAction::from_commands(self.handle_client_snapshot(snapshot.as_ref().clone()))
            }
            AppEvent::Input(input) => self.handle_input_event(*input, now),
        }
    }

    fn handle_input_event(&mut self, input: InputEvent, now: Instant) -> AppAction {
        match input {
            InputEvent::CtrlC => self.handle_close_intent(CloseIntent::CtrlC, now),
            InputEvent::Key(key) if self.screen == AppScreen::Config => {
                self.handle_config_key(key, now)
            }
            InputEvent::Key(key) if self.screen == AppScreen::Sessions => {
                self.handle_sessions_key(key, now)
            }
            InputEvent::Key(key) => {
                self.handle_key_command(key_command(key, self.leader_armed), now)
            }
        }
    }

    /// Update time-sensitive app state.
    pub fn update(&mut self, now: Instant) {
        if self
            .armed_close
            .is_some_and(|armed_close| now >= armed_close.expires_at)
        {
            self.armed_close = None;
        }

        if self
            .notice
            .as_ref()
            .is_some_and(|notice| now >= notice.expires_at)
        {
            self.notice = None;
        }
    }

    /// Return the active notice, if any.
    #[must_use]
    pub fn notice(&self) -> Option<Notice<'_>> {
        if let Some(notice) = &self.notice {
            return Some(Notice::new(notice.text.as_str()));
        }

        self.armed_close
            .map(|armed_close| Notice::new(armed_close.intent.notice()))
    }

    /// Return the active screen.
    #[must_use]
    pub const fn screen(&self) -> AppScreen {
        self.screen
    }

    /// Return the latest shared client snapshot.
    #[must_use]
    pub const fn client_snapshot(&self) -> Option<&ClientSnapshot> {
        self.client_snapshot.as_ref()
    }

    /// Return the active config tab.
    #[must_use]
    pub const fn active_config_tab(&self) -> ConfigTab {
        self.active_config_tab
    }

    /// Return the provider config section.
    #[must_use]
    pub const fn provider_section(&self) -> &ProviderSection {
        &self.provider_section
    }

    /// Return the MCP server config section.
    #[must_use]
    pub const fn mcp_section(&self) -> &McpSection {
        &self.mcp_section
    }

    /// Return the runtime defaults config section.
    #[must_use]
    pub const fn runtime_defaults_section(&self) -> &RuntimeDefaultsSection {
        &self.runtime_defaults_section
    }

    /// Return the sessions screen state.
    #[must_use]
    pub const fn sessions_section(&self) -> &SessionsSection {
        &self.sessions_section
    }

    fn handle_close_intent(&mut self, intent: CloseIntent, now: Instant) -> AppAction {
        self.leader_armed = false;

        if self
            .armed_close
            .is_some_and(|armed_close| armed_close.intent == intent && now < armed_close.expires_at)
        {
            return AppAction::Exit;
        }

        self.armed_close = Some(ArmedClose {
            intent,
            expires_at: now + EXIT_ARM_DURATION,
        });

        AppAction::Continue
    }

    fn handle_key_command(&mut self, command: KeyCommand, now: Instant) -> AppAction {
        match command {
            KeyCommand::None => AppAction::Continue,
            KeyCommand::Escape if self.screen == AppScreen::Config => {
                if self.dismiss_active_overlay() {
                    return AppAction::Continue;
                }

                self.screen = AppScreen::Landing;
                self.leader_armed = false;
                self.armed_close = None;
                AppAction::ClientCommand(ClientCommand::ReleaseConfigWatchInterest)
            }
            KeyCommand::Escape if self.screen == AppScreen::Sessions => {
                let detail_session_id =
                    self.sessions_section.detail_session_id().map(str::to_owned);
                if self.sessions_section.dismiss_route_or_overlay() {
                    if let Some(session_id) = detail_session_id
                        && self.sessions_section.detail_session_id().is_none()
                    {
                        return AppAction::ClientCommand(
                            ClientCommand::ReleaseSessionTimelineWatchInterest { session_id },
                        );
                    }

                    return AppAction::Continue;
                }

                self.screen = AppScreen::Landing;
                self.leader_armed = false;
                self.armed_close = None;
                AppAction::ClientCommand(ClientCommand::ReleaseSessionWatchInterest)
            }
            KeyCommand::Escape => self.handle_close_intent(CloseIntent::Escape, now),
            KeyCommand::Leader => {
                self.leader_armed = true;
                AppAction::Continue
            }
            KeyCommand::LeaderCommand(LeaderCommand::Config) => {
                self.leader_armed = false;

                if self.screen == AppScreen::Config {
                    match self.active_config_tab {
                        ConfigTab::Providers => self.provider_section.open_create_form(),
                        ConfigTab::Mcp => self.mcp_section.open_create_form(),
                        ConfigTab::Defaults => self.runtime_defaults_section.open_create_form(),
                    }
                    return AppAction::Continue;
                }

                if self
                    .client_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.status.ready)
                {
                    let detail_session_id = (self.screen == AppScreen::Sessions)
                        .then(|| self.sessions_section.clear_detail_route())
                        .flatten();
                    self.screen = AppScreen::Config;
                    return AppAction::from_commands(
                        detail_session_id
                            .map(|session_id| {
                                vec![
                                    ClientCommand::ReleaseSessionTimelineWatchInterest {
                                        session_id,
                                    },
                                    ClientCommand::RegisterConfigWatchInterest,
                                ]
                            })
                            .unwrap_or_else(|| vec![ClientCommand::RegisterConfigWatchInterest]),
                    );
                }

                AppAction::Continue
            }
            KeyCommand::LeaderCommand(LeaderCommand::Sessions) => {
                self.leader_armed = false;

                if self.screen == AppScreen::Sessions {
                    return match self.sessions_section.create_session() {
                        SectionAction::ClientCommand(command) => AppAction::ClientCommand(command),
                        SectionAction::Continue | SectionAction::Unhandled => AppAction::Continue,
                    };
                }

                if self
                    .client_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.status.ready)
                {
                    self.screen = AppScreen::Sessions;
                    return AppAction::ClientCommand(ClientCommand::RegisterSessionWatchInterest);
                }

                AppAction::Continue
            }
            KeyCommand::LeaderCommand(LeaderCommand::Quit) => {
                self.leader_armed = false;
                AppAction::Exit
            }
            KeyCommand::LeaderCommand(LeaderCommand::Unknown(command)) => {
                self.leader_armed = false;
                self.notice = Some(TransientNotice {
                    text: format!("unrecognized command: {command}"),
                    expires_at: now + NOTICE_DURATION,
                });

                AppAction::Continue
            }
        }
    }

    fn handle_client_snapshot(&mut self, snapshot: ClientSnapshot) -> Vec<ClientCommand> {
        let detail_before = self.sessions_section.detail_session_id().map(str::to_owned);

        if snapshot.status.ready && self.screen == AppScreen::Waiting {
            self.screen = AppScreen::Landing;
        }

        self.provider_section.reconcile_patch(&snapshot);
        self.mcp_section.reconcile_patch(&snapshot);
        self.runtime_defaults_section.reconcile_patch(&snapshot);
        self.sessions_section.reconcile_operations(&snapshot);

        let provider_count = snapshot
            .config
            .current
            .as_ref()
            .map_or(0, |config| config.providers.len());
        self.provider_section.sync_selection(provider_count);

        let server_count = snapshot
            .config
            .current
            .as_ref()
            .map_or(0, |config| config.mcp_servers.len());
        self.mcp_section.sync_selection(server_count);
        self.runtime_defaults_section
            .sync_selection(snapshot.config.current.as_ref());

        self.sessions_section
            .sync_selection(snapshot.sessions.current.len());

        let detail_after = self.sessions_section.detail_session_id().map(str::to_owned);

        self.client_snapshot = Some(snapshot);

        match (detail_before, detail_after) {
            (None, Some(session_id)) => vec![register_session_timeline_watch_command(session_id)],
            (Some(session_id), None) => {
                vec![ClientCommand::ReleaseSessionTimelineWatchInterest { session_id }]
            }
            (Some(old_session_id), Some(new_session_id)) if old_session_id != new_session_id => {
                vec![
                    ClientCommand::ReleaseSessionTimelineWatchInterest {
                        session_id: old_session_id,
                    },
                    register_session_timeline_watch_command(new_session_id),
                ]
            }
            _ => Vec::new(),
        }
    }

    fn active_section_has_overlay(&self) -> bool {
        match self.active_config_tab {
            ConfigTab::Providers => self.provider_section.has_overlay(),
            ConfigTab::Mcp => self.mcp_section.has_overlay(),
            ConfigTab::Defaults => self.runtime_defaults_section.has_overlay(),
        }
    }

    fn dismiss_active_overlay(&mut self) -> bool {
        match self.active_config_tab {
            ConfigTab::Providers => self.provider_section.dismiss_overlay(),
            ConfigTab::Mcp => self.mcp_section.dismiss_overlay(),
            ConfigTab::Defaults => self.runtime_defaults_section.dismiss_overlay(),
        }
    }

    fn handle_config_key(&mut self, key: KeyEvent, now: Instant) -> AppAction {
        // Tab switches the active config section only while no section overlay
        // is active (overlays consume Tab for their own field navigation).
        if key.code == KeyCode::Tab && !self.active_section_has_overlay() {
            self.active_config_tab = self.active_config_tab.next();
            return AppAction::Continue;
        }

        let command = key_command(key, self.leader_armed);
        if !matches!(command, KeyCommand::None) {
            return self.handle_key_command(command, now);
        }

        let config = self
            .client_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.config.current.as_ref())
            .cloned();
        let action = match self.active_config_tab {
            ConfigTab::Providers => self.provider_section.handle_key(key, config.as_ref()),
            ConfigTab::Mcp => self.mcp_section.handle_key(key, config.as_ref()),
            ConfigTab::Defaults => self
                .runtime_defaults_section
                .handle_key(key, config.as_ref()),
        };
        match action {
            SectionAction::ClientCommand(command) => AppAction::ClientCommand(command),
            SectionAction::Continue | SectionAction::Unhandled => AppAction::Continue,
        }
    }

    fn handle_sessions_key(&mut self, key: KeyEvent, now: Instant) -> AppAction {
        let command = key_command(key, self.leader_armed);
        if !matches!(command, KeyCommand::None) {
            return self.handle_key_command(command, now);
        }

        let detail_before = self.sessions_section.detail_session_id().map(str::to_owned);
        match self
            .sessions_section
            .handle_key(key, self.client_snapshot.as_ref())
        {
            SectionAction::ClientCommand(command) => {
                let detail_after = self.sessions_section.detail_session_id().map(str::to_owned);
                match (detail_before, detail_after) {
                    (Some(old_session_id), Some(new_session_id))
                        if old_session_id != new_session_id =>
                    {
                        AppAction::ClientCommands(vec![
                            ClientCommand::ReleaseSessionTimelineWatchInterest {
                                session_id: old_session_id,
                            },
                            command,
                        ])
                    }
                    _ => AppAction::ClientCommand(command),
                }
            }
            SectionAction::Continue | SectionAction::Unhandled => AppAction::Continue,
        }
    }
}

impl AppAction {
    fn from_commands(commands: Vec<ClientCommand>) -> Self {
        match commands.len() {
            0 => Self::Continue,
            1 => Self::ClientCommand(commands.into_iter().next().expect("one command")),
            _ => Self::ClientCommands(commands),
        }
    }
}

fn register_session_timeline_watch_command(session_id: String) -> ClientCommand {
    ClientCommand::RegisterSessionTimelineWatchInterest {
        session_id,
        after_cursor: None,
        channels: vec![
            "user_text".to_owned(),
            "assistant_text".to_owned(),
            "thinking".to_owned(),
            "tool_call".to_owned(),
            "tool_progress_info".to_owned(),
            "tool_progress_error".to_owned(),
            "tool_result".to_owned(),
            "external_request".to_owned(),
            "metrics".to_owned(),
        ],
        initial_limit: Some(64),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArmedClose {
    intent: CloseIntent,
    expires_at: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TransientNotice {
    text: String,
    expires_at: Instant,
}
