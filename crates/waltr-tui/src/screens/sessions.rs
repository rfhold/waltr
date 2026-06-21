//! Session list/detail surface: state, input, and rendering.

use waltr_client::{
    ClientCommand, ClientOperationView, ClientSnapshot, SessionPhaseView, SessionView,
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
    create: OperationTracker,
    delete: OperationTracker,
}

impl SessionsSection {
    /// Create an empty sessions section.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected_session: 0,
            detail_session_id: None,
            delete_session_id: None,
            create: OperationTracker::new(),
            delete: OperationTracker::new(),
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

        if self.delete_session_id.take().is_some() {
            self.delete.clear_error();
            return true;
        }

        self.detail_session_id.take().is_some()
    }

    /// Start creating a session.
    pub fn create_session(&mut self) -> SectionAction {
        if self.create.is_pending() || self.delete.is_pending() {
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
        if self.create.is_pending() || self.delete.is_pending() {
            return SectionAction::Continue;
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

fn sorted_sessions(snapshot: &ClientSnapshot) -> Vec<SessionView> {
    sorted_sessions_from_slice(&snapshot.sessions.current)
}

fn sorted_sessions_from_slice(sessions: &[SessionView]) -> Vec<SessionView> {
    let mut sessions = sessions.to_vec();
    sessions.sort_by(|left, right| session_timestamp(right).cmp(&session_timestamp(left)));
    sessions
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

    let Some(session) = session else {
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

    let delete_line = if section.delete_session_id.as_deref() == Some(session.id.as_str()) {
        Line::from(vec![ui::delete_prompt_span(ui::delete_prompt_text(
            section.delete.is_pending(),
            section.delete.error(),
        ))])
    } else {
        Line::from(vec![
            Span::styled("Delete", THEME.danger_style().add_modifier(Modifier::BOLD)),
            Span::raw("  Backspace/Delete"),
        ])
    };

    Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Sessions", THEME.muted_text_style()),
            Span::raw(" / "),
            Span::raw(session.id.clone()),
        ]),
        Line::from(""),
        field_line("id", session.id.clone()),
        field_line("revision", session.revision.to_string()),
        Line::from(vec![
            Span::styled("phase", THEME.muted_text_style()),
            Span::raw("  "),
            phase_span(session.status.phase),
        ]),
        field_line(
            "created",
            format_optional_timestamp(session.created_at.as_ref()),
        ),
        field_line("updated", format_timestamp(&session)),
        field_line(
            "last activity",
            format_optional_timestamp(session.last_activity_at.as_ref()),
        ),
        failure_line(&session),
        Line::from(""),
        delete_line,
    ])
    .style(THEME.text_style())
    .wrap(Wrap { trim: false })
}

fn status_widget(
    snapshot: Option<&ClientSnapshot>,
    section: &SessionsSection,
) -> Paragraph<'static> {
    let error = section
        .create
        .error()
        .or(section.delete.error())
        .or(session_error(snapshot));

    let lines = error.map_or_else(Vec::new, |error| vec![ui::error_line(error)]);
    Paragraph::new(lines).style(THEME.text_style())
}

fn field_line(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, THEME.muted_text_style()),
        Span::raw("  "),
        Span::raw(value),
    ])
}

fn failure_line(session: &SessionView) -> Line<'static> {
    if session.status.phase == SessionPhaseView::Failed && !session.status.failed_message.is_empty()
    {
        return field_line("failure", session.status.failed_message.clone());
    }

    Line::from("")
}

fn phase_span(phase: SessionPhaseView) -> Span<'static> {
    let role = match phase {
        SessionPhaseView::Failed => StatusRole::Error,
        SessionPhaseView::Finished => StatusRole::Ready,
        SessionPhaseView::Idle | SessionPhaseView::Suspended => StatusRole::Ready,
        _ => StatusRole::Loading,
    };

    Span::styled(
        phase_name(phase),
        THEME.status_style(role).add_modifier(Modifier::BOLD),
    )
}

fn phase_name(phase: SessionPhaseView) -> &'static str {
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
