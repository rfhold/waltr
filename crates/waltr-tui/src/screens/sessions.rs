//! Session list/detail surface: state, input, and rendering.

use std::collections::BTreeSet;

use waltr_client::{
    ClientCommand, ClientOperationView, ClientSnapshot, ConfigView, ProviderView,
    RuntimeConfigMatchView, SessionPhaseView, SessionRuntimeConfigView, SessionView,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    screens::{OperationResult, OperationTracker, SectionAction},
    theme::{StatusRole, THEME},
    ui,
};

/// Self-contained sessions screen state and behavior.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionsSection {
    selected_session: usize,
    detail_session_id: Option<String>,
    delete_session_id: Option<String>,
    runtime_editor: Option<RuntimeEditor>,
    pending_runtime_editor: Option<RuntimeEditorKind>,
    create: OperationTracker,
    delete: OperationTracker,
    runtime_update: OperationTracker,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RuntimeEditor {
    Provider {
        query: String,
        selected_provider: usize,
        selected_model: usize,
    },
    Mcp {
        query: String,
        selected: usize,
        enabled: BTreeSet<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeEditorKind {
    Provider,
    Mcp,
}

impl SessionsSection {
    /// Create an empty sessions section.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected_session: 0,
            detail_session_id: None,
            delete_session_id: None,
            runtime_editor: None,
            pending_runtime_editor: None,
            create: OperationTracker::new(),
            delete: OperationTracker::new(),
            runtime_update: OperationTracker::new(),
        }
    }

    /// Return the selected session index.
    #[must_use]
    pub const fn selected_session(&self) -> usize {
        self.selected_session
    }

    /// Return the active detail session id, if any.
    #[must_use]
    pub fn detail_session_id(&self) -> Option<&str> {
        self.detail_session_id.as_deref()
    }

    /// Whether this section is showing a detail route.
    #[must_use]
    pub const fn in_detail(&self) -> bool {
        self.detail_session_id.is_some()
    }

    /// Reconcile submitted create/delete operations against shared reducer state.
    pub fn reconcile_operations(&mut self, snapshot: &ClientSnapshot) {
        if let Some(Ok(Some(session))) = self.create.reconcile(
            &snapshot.sessions.create,
            snapshot,
            |operation| match operation {
                ClientOperationView::SessionCreate {
                    status,
                    error,
                    session,
                } => Some(match *status {
                    "ready" => OperationResult::Ready(session.clone()),
                    "failed" => OperationResult::Failed(
                        error
                            .as_deref()
                            .unwrap_or("session create failed")
                            .to_owned(),
                    ),
                    _ => OperationResult::Pending,
                }),
                _ => None,
            },
        ) {
            self.detail_session_id = Some(session.id.clone());
            self.select_session_by_id(&snapshot.sessions.current, &session.id);
        }

        if let Some(Ok(id)) = self.delete.reconcile(
            &snapshot.sessions.delete,
            snapshot,
            |operation| match operation {
                ClientOperationView::SessionDelete { status, error, id } => Some(match *status {
                    "ready" => OperationResult::Ready(id.clone()),
                    "failed" => OperationResult::Failed(
                        error
                            .as_deref()
                            .unwrap_or("session delete failed")
                            .to_owned(),
                    ),
                    _ => OperationResult::Pending,
                }),
                _ => None,
            },
        ) {
            self.delete_session_id = None;
            if self.detail_session_id.as_deref() == Some(id.as_str()) {
                self.detail_session_id = None;
            }
        }

        if let Some(Ok(_session)) = self.runtime_update.reconcile(
            &snapshot.sessions.runtime_config_update,
            snapshot,
            |operation| match operation {
                ClientOperationView::SessionRuntimeConfigUpdate {
                    status,
                    error,
                    session,
                } => Some(match *status {
                    "ready" => OperationResult::Ready(session.clone()),
                    "failed" => OperationResult::Failed(
                        error
                            .as_deref()
                            .unwrap_or("session runtime update failed")
                            .to_owned(),
                    ),
                    _ => OperationResult::Pending,
                }),
                _ => None,
            },
        ) {
            self.runtime_editor = None;
        }

        if let Some(kind) = self.pending_runtime_editor.take() {
            match kind {
                RuntimeEditorKind::Provider => self.open_provider_editor(Some(snapshot)),
                RuntimeEditorKind::Mcp => self.open_mcp_editor(Some(snapshot)),
            }

            if self.runtime_editor.is_none() {
                self.pending_runtime_editor = Some(kind);
            }
        }
    }

    /// Clamp selection after a snapshot update.
    pub const fn sync_selection(&mut self, session_count: usize) {
        if session_count == 0 {
            self.selected_session = 0;
        } else if self.selected_session >= session_count {
            self.selected_session = session_count - 1;
        }
    }

    /// Dismiss the active detail route or delete confirmation. Returns true when handled.
    pub fn dismiss_route_or_overlay(&mut self) -> bool {
        if self.delete.is_pending() || self.create.is_pending() {
            return true;
        }

        if self.runtime_editor.take().is_some() {
            self.runtime_update.clear_error();
            return true;
        }

        if self.pending_runtime_editor.take().is_some() {
            return true;
        }

        if self.delete_session_id.take().is_some() {
            self.delete.clear_error();
            return true;
        }

        self.detail_session_id.take().is_some()
    }

    /// Start creating a session.
    pub fn create_session(&mut self) -> SectionAction {
        if self.create.is_pending() || self.delete.is_pending() || self.runtime_update.is_pending()
        {
            return SectionAction::Continue;
        }

        self.create.begin();
        SectionAction::ClientCommand(ClientCommand::CreateSession)
    }

    /// Route a key event into the sessions section.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if self.create.is_pending() || self.delete.is_pending() || self.runtime_update.is_pending()
        {
            return SectionAction::Continue;
        }

        if self.runtime_editor.is_some() {
            return self.handle_runtime_editor_key(key, snapshot);
        }

        if let Some(id) = self.delete_session_id.clone() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.delete_selected(&id),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.delete_session_id = None;
                    self.delete.clear_error();
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            };
        }

        if self.detail_session_id.is_some() {
            match key.code {
                KeyCode::Char('p') => {
                    return self.open_runtime_editor(RuntimeEditorKind::Provider, snapshot);
                }
                KeyCode::Char('m') => {
                    return self.open_runtime_editor(RuntimeEditorKind::Mcp, snapshot);
                }
                _ => {}
            }
        }

        let sessions = snapshot.map_or_else(Vec::new, sorted_sessions);
        match key.code {
            KeyCode::Up => {
                self.selected_session = self.selected_session.saturating_sub(1);
                SectionAction::Continue
            }
            KeyCode::Down => {
                let max = sessions.len().saturating_sub(1);
                self.selected_session = (self.selected_session + 1).min(max);
                SectionAction::Continue
            }
            KeyCode::Right | KeyCode::Enter => {
                if let Some(session) = sessions.get(self.selected_session) {
                    self.detail_session_id = Some(session.id.clone());
                }
                SectionAction::Continue
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.arm_delete(snapshot);
                SectionAction::Continue
            }
            _ => SectionAction::Unhandled,
        }
    }

    fn open_runtime_editor(
        &mut self,
        kind: RuntimeEditorKind,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if config(snapshot).is_none() {
            self.pending_runtime_editor = Some(kind);
            return SectionAction::ClientCommand(ClientCommand::RequestConfig);
        }

        match kind {
            RuntimeEditorKind::Provider => self.open_provider_editor(snapshot),
            RuntimeEditorKind::Mcp => self.open_mcp_editor(snapshot),
        }

        SectionAction::Continue
    }

    #[allow(clippy::too_many_lines)]
    fn handle_runtime_editor_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        let detail_session = self.current_detail_session(snapshot);
        let Some(editor) = &mut self.runtime_editor else {
            return SectionAction::Unhandled;
        };

        match editor {
            RuntimeEditor::Provider {
                query,
                selected_provider,
                selected_model,
            } => match key.code {
                KeyCode::Esc => {
                    self.runtime_editor = None;
                    SectionAction::Continue
                }
                KeyCode::Up => {
                    *selected_provider = selected_provider.saturating_sub(1);
                    *selected_model = 0;
                    SectionAction::Continue
                }
                KeyCode::Down => {
                    let max = provider_entries(snapshot).len().saturating_sub(1);
                    *selected_provider = (*selected_provider + 1).min(max);
                    *selected_model = 0;
                    SectionAction::Continue
                }
                KeyCode::Left => {
                    *selected_model = selected_model.saturating_sub(1);
                    SectionAction::Continue
                }
                KeyCode::Right => {
                    let max = provider_entries(snapshot)
                        .get(*selected_provider)
                        .map_or(0, |(_, provider)| provider.models.len().saturating_sub(1));
                    *selected_model = (*selected_model + 1).min(max);
                    SectionAction::Continue
                }
                KeyCode::Enter => self.save_provider_runtime(snapshot),
                KeyCode::Backspace => {
                    query.pop();
                    *selected_provider = 0;
                    *selected_model = 0;
                    SectionAction::Continue
                }
                KeyCode::Char(character) => {
                    query.push(character);
                    *selected_provider = 0;
                    *selected_model = 0;
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            },
            RuntimeEditor::Mcp {
                query,
                selected,
                enabled,
            } => match key.code {
                KeyCode::Esc => {
                    self.runtime_editor = None;
                    SectionAction::Continue
                }
                KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    SectionAction::Continue
                }
                KeyCode::Down => {
                    let max = mcp_entries(snapshot, detail_session.as_ref())
                        .len()
                        .saturating_sub(1);
                    *selected = (*selected + 1).min(max);
                    SectionAction::Continue
                }
                KeyCode::Char(' ') => {
                    if let Some(item) =
                        mcp_entries(snapshot, detail_session.as_ref()).get(*selected)
                        && item.configured
                    {
                        if enabled.contains(&item.key) {
                            enabled.remove(&item.key);
                        } else {
                            enabled.insert(item.key.clone());
                        }
                    }
                    SectionAction::Continue
                }
                KeyCode::Enter => self.save_mcp_runtime(snapshot),
                KeyCode::Backspace => {
                    query.pop();
                    *selected = 0;
                    SectionAction::Continue
                }
                KeyCode::Char(character) => {
                    query.push(character);
                    *selected = 0;
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            },
        }
    }

    fn open_provider_editor(&mut self, snapshot: Option<&ClientSnapshot>) {
        if provider_entries(snapshot).is_empty() {
            return;
        }
        self.runtime_update.clear_error();
        self.runtime_editor = Some(RuntimeEditor::Provider {
            query: String::new(),
            selected_provider: 0,
            selected_model: 0,
        });
    }

    fn open_mcp_editor(&mut self, snapshot: Option<&ClientSnapshot>) {
        let enabled = self
            .current_detail_session(snapshot)
            .and_then(|session| session.runtime_config)
            .map_or_else(BTreeSet::new, |runtime| {
                runtime
                    .mcp_servers
                    .into_iter()
                    .map(|server| server.key)
                    .collect()
            });
        self.runtime_update.clear_error();
        self.runtime_editor = Some(RuntimeEditor::Mcp {
            query: String::new(),
            selected: 0,
            enabled,
        });
    }

    fn save_provider_runtime(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(session) = self.current_detail_session(snapshot) else {
            return SectionAction::Continue;
        };
        let Some(RuntimeEditor::Provider {
            selected_provider,
            selected_model,
            ..
        }) = &self.runtime_editor
        else {
            return SectionAction::Continue;
        };
        let Some((provider_key, provider)) =
            provider_entries(snapshot).get(*selected_provider).cloned()
        else {
            return SectionAction::Continue;
        };
        let Some(model_id) = sorted_model_keys(&provider).get(*selected_model).cloned() else {
            return SectionAction::Continue;
        };
        let enabled_mcp_server_keys =
            session
                .runtime_config
                .as_ref()
                .map_or_else(Vec::new, |runtime| {
                    runtime
                        .mcp_servers
                        .iter()
                        .map(|server| server.key.clone())
                        .collect()
                });

        self.runtime_update.begin();
        SectionAction::ClientCommand(ClientCommand::UpdateSessionRuntimeConfig {
            id: session.id,
            provider_key,
            model_id,
            enabled_mcp_server_keys,
        })
    }

    fn save_mcp_runtime(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(session) = self.current_detail_session(snapshot) else {
            return SectionAction::Continue;
        };
        let Some(provider) = session
            .runtime_config
            .as_ref()
            .and_then(|runtime| runtime.provider.as_ref())
        else {
            return SectionAction::Continue;
        };
        let Some(RuntimeEditor::Mcp { enabled, .. }) = &self.runtime_editor else {
            return SectionAction::Continue;
        };

        self.runtime_update.begin();
        SectionAction::ClientCommand(ClientCommand::UpdateSessionRuntimeConfig {
            id: session.id,
            provider_key: provider.key.clone(),
            model_id: provider.model_id.clone(),
            enabled_mcp_server_keys: enabled.iter().cloned().collect(),
        })
    }

    fn current_detail_session(&self, snapshot: Option<&ClientSnapshot>) -> Option<SessionView> {
        let id = self.detail_session_id()?;
        sorted_sessions_from_slice(&snapshot?.sessions.current)
            .into_iter()
            .find(|session| session.id == id)
    }

    fn arm_delete(&mut self, snapshot: Option<&ClientSnapshot>) {
        self.delete.clear_error();
        self.delete_session_id = if let Some(id) = &self.detail_session_id {
            Some(id.clone())
        } else {
            snapshot
                .map_or_else(Vec::new, sorted_sessions)
                .get(self.selected_session)
                .map(|session| session.id.clone())
        };
    }

    fn delete_selected(&mut self, id: &str) -> SectionAction {
        self.delete.begin();
        SectionAction::ClientCommand(ClientCommand::DeleteSession { id: id.to_owned() })
    }

    fn select_session_by_id(&mut self, sessions: &[SessionView], id: &str) {
        if let Some(index) = sorted_sessions_from_slice(sessions)
            .iter()
            .position(|session| session.id == id)
        {
            self.selected_session = index;
        }
    }

    /// Render the sessions screen into `area`.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, snapshot: Option<&ClientSnapshot>) {
        if self.in_detail() {
            self.render_detail(frame, area, snapshot);
            return;
        }

        let modal_area = ui::centered_percent_rect(area, 82, 18);
        frame.render_widget(Clear, modal_area);

        let block = ui::modal_block(
            " Sessions ",
            " Esc back  C-x s create  Right/Enter detail  Backspace/Delete delete ",
        )
        .border_style(THEME.accent_style());
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let [header_area, content_area, status_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(status_height(snapshot, self)),
            ])
            .areas(inner);

        frame.render_widget(header_widget(self), header_area);

        if self.in_detail() {
            frame.render_widget(detail_widget(self, snapshot), content_area);
        } else if is_pending(snapshot) {
            frame.render_widget(loading_widget(snapshot), content_area);
        } else {
            frame.render_widget(list_widget(self, snapshot), content_area);
        }

        if status_height(snapshot, self) > 0 {
            frame.render_widget(status_widget(snapshot, self), status_area);
        }
    }

    fn render_detail(&self, frame: &mut Frame<'_>, area: Rect, snapshot: Option<&ClientSnapshot>) {
        frame.render_widget(Clear, area);

        let [content_area, status_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .areas(area);

        let [detail_area, error_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(status_height(snapshot, self)),
            ])
            .areas(content_area);

        frame.render_widget(detail_widget(self, snapshot), detail_area);

        if status_height(snapshot, self) > 0 {
            frame.render_widget(status_widget(snapshot, self), error_area);
        }

        frame.render_widget(detail_status_widget(self, snapshot), status_area);

        if self.runtime_editor.is_some() {
            render_runtime_editor(frame, area, self, snapshot);
        }
    }
}

/// Render the sessions surface.
pub fn draw_sessions(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) {
    section.render(frame, area, snapshot);
}

fn render_runtime_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) {
    let modal_area = ui::centered_percent_rect(area, 62, 42);
    frame.render_widget(Clear, modal_area);

    let title = match section.runtime_editor {
        Some(RuntimeEditor::Provider { .. }) => " Edit provider/model ",
        Some(RuntimeEditor::Mcp { .. }) => " Edit MCP servers ",
        None => return,
    };
    let block = ui::modal_block(title, " Esc cancel  type search  Enter save ")
        .border_style(THEME.accent_style());
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let lines = match &section.runtime_editor {
        Some(RuntimeEditor::Provider {
            query,
            selected_provider,
            selected_model,
        }) => provider_editor_lines(snapshot, query, *selected_provider, *selected_model),
        Some(RuntimeEditor::Mcp {
            query,
            selected,
            enabled,
        }) => mcp_editor_lines(
            snapshot,
            section.current_detail_session(snapshot).as_ref(),
            query,
            *selected,
            enabled,
        ),
        None => Vec::new(),
    };

    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.text_style())
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn provider_editor_lines(
    snapshot: Option<&ClientSnapshot>,
    query: &str,
    selected_provider: usize,
    selected_model: usize,
) -> Vec<Line<'static>> {
    let entries = filtered_provider_entries(snapshot, query);
    let mut lines = vec![
        Line::from("Updates session metadata immediately; next prompt uses this runtime."),
        Line::from(vec![
            Span::styled("search ", THEME.muted_text_style()),
            Span::raw(query.to_owned()),
        ]),
        Line::from(vec![Span::styled("providers", THEME.accent_bold())]),
    ];

    for (index, (key, provider)) in entries.iter().enumerate() {
        lines.push(Line::from(vec![
            ui::selection_marker(index == selected_provider),
            Span::raw(" "),
            ui::selected_title(key.clone()),
            Span::raw("  "),
            Span::styled(provider.name.clone(), THEME.muted_text_style()),
        ]));
    }

    if let Some((_, provider)) = entries.get(selected_provider) {
        lines.push(Line::from(vec![Span::styled(
            "models",
            THEME.accent_bold(),
        )]));
        for (index, model) in sorted_model_keys(provider).iter().enumerate() {
            lines.push(Line::from(vec![
                ui::selection_marker(index == selected_model),
                Span::raw(" "),
                Span::raw(model.clone()),
            ]));
        }
    }

    lines
}

fn mcp_editor_lines(
    snapshot: Option<&ClientSnapshot>,
    session: Option<&SessionView>,
    query: &str,
    selected: usize,
    enabled: &BTreeSet<String>,
) -> Vec<Line<'static>> {
    let entries = filtered_mcp_entries(snapshot, session, query);
    let mut lines = vec![
        Line::from("Space toggles configured servers. Missing servers are shown for drift."),
        Line::from(vec![
            Span::styled("search ", THEME.muted_text_style()),
            Span::raw(query.to_owned()),
        ]),
    ];

    for (index, entry) in entries.iter().enumerate() {
        let marker = if enabled.contains(&entry.key) {
            "[x]"
        } else {
            "[ ]"
        };
        lines.push(Line::from(vec![
            ui::selection_marker(index == selected),
            Span::raw(" "),
            Span::raw(marker),
            Span::raw(" "),
            ui::selected_title(entry.key.clone()),
            Span::raw("  "),
            runtime_match_span(entry.state),
        ]));
    }

    lines
}

fn sorted_sessions(snapshot: &ClientSnapshot) -> Vec<SessionView> {
    sorted_sessions_from_slice(&snapshot.sessions.current)
}

fn sorted_sessions_from_slice(sessions: &[SessionView]) -> Vec<SessionView> {
    let mut sessions = sessions.to_vec();
    sessions.sort_by_key(|session| std::cmp::Reverse(session_timestamp(session)));
    sessions
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct McpEntry {
    key: String,
    configured: bool,
    state: RuntimeConfigMatchView,
}

fn config(snapshot: Option<&ClientSnapshot>) -> Option<&ConfigView> {
    snapshot.and_then(|snapshot| snapshot.config.current.as_ref())
}

fn provider_entries(snapshot: Option<&ClientSnapshot>) -> Vec<(String, ProviderView)> {
    filtered_provider_entries(snapshot, "")
}

fn filtered_provider_entries(
    snapshot: Option<&ClientSnapshot>,
    query: &str,
) -> Vec<(String, ProviderView)> {
    let needle = query.trim().to_lowercase();
    let mut entries = config(snapshot).map_or_else(Vec::new, |config| {
        config
            .providers
            .iter()
            .filter(|(key, provider)| {
                needle.is_empty()
                    || key.to_lowercase().contains(&needle)
                    || provider.name.to_lowercase().contains(&needle)
            })
            .map(|(key, provider)| (key.clone(), provider.clone()))
            .collect::<Vec<_>>()
    });
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn mcp_entries(snapshot: Option<&ClientSnapshot>, session: Option<&SessionView>) -> Vec<McpEntry> {
    filtered_mcp_entries(snapshot, session, "")
}

fn filtered_mcp_entries(
    snapshot: Option<&ClientSnapshot>,
    session: Option<&SessionView>,
    query: &str,
) -> Vec<McpEntry> {
    let needle = query.trim().to_lowercase();
    let mut entries = config(snapshot).map_or_else(Vec::new, |config| {
        config
            .mcp_servers
            .keys()
            .filter(|key| needle.is_empty() || key.to_lowercase().contains(&needle))
            .map(|key| McpEntry {
                key: key.clone(),
                configured: true,
                state: session
                    .and_then(|session| session.runtime_config.as_ref())
                    .and_then(|runtime| {
                        runtime.mcp_servers.iter().find(|server| server.key == *key)
                    })
                    .map_or(RuntimeConfigMatchView::Configured, |server| {
                        server.match_status
                    }),
            })
            .collect::<Vec<_>>()
    });

    if let Some(runtime) = session.and_then(|session| session.runtime_config.as_ref()) {
        for server in &runtime.mcp_servers {
            if !entries.iter().any(|entry| entry.key == server.key)
                && (needle.is_empty() || server.key.to_lowercase().contains(&needle))
            {
                entries.push(McpEntry {
                    key: server.key.clone(),
                    configured: false,
                    state: server.match_status,
                });
            }
        }
    }

    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

fn sorted_model_keys(provider: &ProviderView) -> Vec<String> {
    let mut keys = provider.models.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn session_timestamp(session: &SessionView) -> i64 {
    session
        .updated_at
        .as_ref()
        .or(session.last_activity_at.as_ref())
        .or(session.created_at.as_ref())
        .map_or(0, |timestamp| timestamp.seconds)
}

fn is_pending(snapshot: Option<&ClientSnapshot>) -> bool {
    let Some(snapshot) = snapshot else {
        return true;
    };

    snapshot.sessions.current.is_empty()
        && snapshot.sessions.list.error.is_none()
        && snapshot.sessions.watch.error.is_none()
        && snapshot.sessions.list.status != "ready"
        && !matches!(snapshot.sessions.watch.status, "opened" | "received")
}

fn status_height(snapshot: Option<&ClientSnapshot>, section: &SessionsSection) -> u16 {
    u16::from(
        session_error(snapshot).is_some()
            || section.create.error().is_some()
            || section.delete.error().is_some(),
    )
}

fn session_error(snapshot: Option<&ClientSnapshot>) -> Option<&str> {
    snapshot.and_then(|snapshot| {
        snapshot
            .sessions
            .watch
            .error
            .as_deref()
            .or(snapshot.sessions.list.error.as_deref())
    })
}

fn header_widget(section: &SessionsSection) -> Paragraph<'static> {
    let title = if section.in_detail() {
        "Session detail"
    } else {
        "Recent sessions"
    };
    let create = if section.create.is_pending() {
        "Creating..."
    } else {
        "Create: C-x s"
    };

    Paragraph::new(vec![
        Line::from(vec![Span::styled("Sessions", THEME.accent_bold())]),
        Line::from(vec![
            Span::styled(title, THEME.text_style()),
            Span::raw("  "),
            Span::styled(create, THEME.focus_style()),
        ]),
    ])
}

fn loading_widget(snapshot: Option<&ClientSnapshot>) -> Paragraph<'static> {
    let watch = snapshot.map_or("idle", |snapshot| snapshot.sessions.watch.status);

    Paragraph::new(vec![
        ui::loading_line("Loading sessions..."),
        Line::from(vec![
            Span::styled("watch ", THEME.muted_text_style()),
            Span::styled(watch.to_owned(), THEME.focus_style()),
        ]),
    ])
    .style(THEME.text_style())
}

fn list_widget(section: &SessionsSection, snapshot: Option<&ClientSnapshot>) -> Paragraph<'static> {
    let sessions = snapshot.map_or_else(Vec::new, sorted_sessions);
    let lines = if sessions.is_empty() {
        vec![ui::loading_line("No sessions yet.")]
    } else {
        sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                session_line(section, session, index == section.selected_session)
            })
            .collect()
    };

    Paragraph::new(lines)
        .style(THEME.text_style())
        .wrap(Wrap { trim: false })
}

fn session_line(section: &SessionsSection, session: &SessionView, selected: bool) -> Line<'static> {
    if section.delete_session_id.as_deref() == Some(session.id.as_str()) {
        return Line::from(vec![
            ui::selection_marker(selected),
            Span::raw(" "),
            ui::selected_title(session.id.clone()),
            Span::raw("  "),
            ui::delete_prompt_span(ui::delete_prompt_text(
                section.delete.is_pending(),
                section.delete.error(),
            )),
        ]);
    }

    Line::from(vec![
        ui::selection_marker(selected),
        Span::raw(" "),
        ui::selected_title(session.id.clone()),
        Span::raw("  "),
        phase_span(session.status.phase),
        Span::raw("  "),
        Span::styled(format_timestamp(session), THEME.muted_text_style()),
        runtime_warning_span(session),
    ])
}

fn detail_widget(
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) -> Paragraph<'static> {
    let Some(id) = section.detail_session_id() else {
        return Paragraph::new(Vec::<Line<'static>>::new()).style(THEME.text_style());
    };

    let session = snapshot
        .map_or_else(Vec::new, sorted_sessions)
        .into_iter()
        .find(|session| session.id == id);

    let Some(_session) = session else {
        return Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Sessions", THEME.muted_text_style()),
                Span::raw(" / "),
                Span::raw(id.to_owned()),
            ]),
            ui::loading_line("Waiting for session..."),
        ])
        .style(THEME.text_style());
    };

    Paragraph::new(Vec::<Line<'static>>::new())
        .style(THEME.text_style())
        .wrap(Wrap { trim: false })
}

fn detail_status_widget(
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) -> Paragraph<'static> {
    let Some(id) = section.detail_session_id() else {
        return Paragraph::new(Line::from(Span::styled(
            "status unknown  provider none  mcp none",
            THEME.muted_text_style(),
        )));
    };

    let session = snapshot
        .map_or_else(Vec::new, sorted_sessions)
        .into_iter()
        .find(|session| session.id == id);

    let Some(session) = session else {
        return Paragraph::new(Line::from(vec![
            Span::styled("status waiting", THEME.status_style(StatusRole::Loading)),
            Span::raw("  "),
            Span::styled("provider none", THEME.muted_text_style()),
            Span::raw("  "),
            Span::styled("mcp none", THEME.muted_text_style()),
        ]));
    };

    Paragraph::new(Line::from(vec![
        Span::styled("status ", THEME.muted_text_style()),
        phase_span(session.status.phase),
        Span::raw("  "),
        Span::styled("provider ", THEME.muted_text_style()),
        Span::raw(provider_status_text(session.runtime_config.as_ref())),
        Span::raw(" "),
        Span::styled("(p)", THEME.focus_style()),
        Span::raw("  "),
        Span::styled("mcp ", THEME.muted_text_style()),
        Span::raw(mcp_status_text(session.runtime_config.as_ref())),
        Span::raw(" "),
        Span::styled("(m)", THEME.focus_style()),
        Span::raw("  "),
        delete_status_span(section, &session),
    ]))
    .style(THEME.text_style())
}

fn delete_status_span(section: &SessionsSection, session: &SessionView) -> Span<'static> {
    if section.delete_session_id.as_deref() == Some(session.id.as_str()) {
        return ui::delete_prompt_span(ui::delete_prompt_text(
            section.delete.is_pending(),
            section.delete.error(),
        ));
    }

    Span::styled(
        "delete Backspace/Delete",
        THEME.danger_style().add_modifier(Modifier::BOLD),
    )
}

fn provider_status_text(runtime: Option<&SessionRuntimeConfigView>) -> String {
    runtime
        .and_then(|runtime| runtime.provider.as_ref())
        .map_or_else(
            || "none".to_owned(),
            |provider| format!("{}/{}", provider.key, provider.model_id),
        )
}

fn mcp_status_text(runtime: Option<&SessionRuntimeConfigView>) -> String {
    let Some(runtime) = runtime else {
        return "none".to_owned();
    };

    if runtime.mcp_servers.is_empty() {
        return "none".to_owned();
    }

    runtime
        .mcp_servers
        .iter()
        .map(|server| server.key.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn runtime_warning_span(session: &SessionView) -> Span<'static> {
    let Some(runtime) = &session.runtime_config else {
        return Span::raw("");
    };
    let provider_mismatch = runtime.provider.as_ref().is_some_and(|provider| {
        matches!(
            provider.match_status,
            RuntimeConfigMatchView::Missing | RuntimeConfigMatchView::Changed
        )
    });
    let mcp_mismatch = runtime.mcp_servers.iter().any(|server| {
        matches!(
            server.match_status,
            RuntimeConfigMatchView::Missing | RuntimeConfigMatchView::Changed
        )
    });
    if provider_mismatch || mcp_mismatch {
        Span::styled("  runtime drift", THEME.status_style(StatusRole::Saving))
    } else {
        Span::raw("")
    }
}

fn runtime_match_span(state: RuntimeConfigMatchView) -> Span<'static> {
    let (label, role) = match state {
        RuntimeConfigMatchView::Configured => ("configured", StatusRole::Ready),
        RuntimeConfigMatchView::Missing => ("missing", StatusRole::Error),
        RuntimeConfigMatchView::Changed => ("changed", StatusRole::Saving),
        RuntimeConfigMatchView::Unspecified | RuntimeConfigMatchView::Unknown => {
            ("unknown", StatusRole::Loading)
        }
    };

    Span::styled(label, THEME.status_style(role).add_modifier(Modifier::BOLD))
}

fn status_widget(
    snapshot: Option<&ClientSnapshot>,
    section: &SessionsSection,
) -> Paragraph<'static> {
    let error = section
        .create
        .error()
        .or_else(|| section.delete.error())
        .or_else(|| session_error(snapshot));

    let lines = error.map_or_else(Vec::new, |error| vec![ui::error_line(error)]);
    Paragraph::new(lines).style(THEME.text_style())
}

fn phase_span(phase: SessionPhaseView) -> Span<'static> {
    let role = match phase {
        SessionPhaseView::Failed => StatusRole::Error,
        SessionPhaseView::Finished | SessionPhaseView::Idle | SessionPhaseView::Suspended => {
            StatusRole::Ready
        }
        _ => StatusRole::Loading,
    };

    Span::styled(
        phase_name(phase),
        THEME.status_style(role).add_modifier(Modifier::BOLD),
    )
}

const fn phase_name(phase: SessionPhaseView) -> &'static str {
    match phase {
        SessionPhaseView::Unspecified => "unspecified",
        SessionPhaseView::Idle => "idle",
        SessionPhaseView::BuildingRequest => "building request",
        SessionPhaseView::StreamingProvider => "streaming provider",
        SessionPhaseView::EvaluatingToolCall => "evaluating tool call",
        SessionPhaseView::RunningTool => "running tool",
        SessionPhaseView::Suspended => "suspended",
        SessionPhaseView::Finished => "finished",
        SessionPhaseView::Failed => "failed",
        SessionPhaseView::Unknown => "unknown",
    }
}

fn format_timestamp(session: &SessionView) -> String {
    format_optional_timestamp(
        session
            .updated_at
            .as_ref()
            .or(session.last_activity_at.as_ref())
            .or(session.created_at.as_ref()),
    )
}

fn format_optional_timestamp(timestamp: Option<&waltr_client::TimestampView>) -> String {
    timestamp.map_or_else(
        || "No updates".to_owned(),
        |timestamp| format!("{}s", timestamp.seconds),
    )
}
