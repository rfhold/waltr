//! Destructive session-data wipe screen.

use waltr_client::{ClientOperationView, ClientSnapshot};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    screens::{OperationResult, OperationTracker},
    theme::THEME,
    ui,
};

/// Input outcome from the session-data screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionDataAction {
    Continue,
    Cancel,
    Confirm,
}

/// Terminal-local confirmation and operation state for wiping sessions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionDataState {
    operation: OperationTracker,
    succeeded: bool,
}

impl SessionDataState {
    pub(crate) const fn new() -> Self {
        Self {
            operation: OperationTracker::new(),
            succeeded: false,
        }
    }

    pub(crate) fn open(&mut self) {
        self.operation.clear();
        self.succeeded = false;
    }

    pub(crate) const fn is_pending(&self) -> bool {
        self.operation.is_pending()
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.operation.error()
    }

    pub(crate) const fn succeeded(&self) -> bool {
        self.succeeded
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> SessionDataAction {
        if self.operation.is_pending() {
            if !self.operation.has_key() && matches!(key.code, KeyCode::Esc | KeyCode::Char('n')) {
                self.operation.clear();
                return SessionDataAction::Cancel;
            }
            return SessionDataAction::Continue;
        }

        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                self.operation.begin();
                self.succeeded = false;
                SessionDataAction::Confirm
            }
            KeyCode::Esc | KeyCode::Char('n') => SessionDataAction::Cancel,
            _ => SessionDataAction::Continue,
        }
    }

    pub(crate) fn reconcile(&mut self, snapshot: &ClientSnapshot) {
        let result = self
            .operation
            .reconcile(&snapshot.sessions.wipe, snapshot, |operation| {
                let ClientOperationView::SessionWipe { status, error } = operation else {
                    return None;
                };
                Some(match *status {
                    "ready" => OperationResult::Ready(()),
                    "failed" => OperationResult::Failed(
                        error
                            .clone()
                            .unwrap_or_else(|| "session data wipe failed".to_owned()),
                    ),
                    _ => OperationResult::Pending,
                })
            });
        if result.as_ref().is_some_and(Result::is_ok) {
            self.succeeded = true;
        }
    }
}

/// Render the dedicated session-data wipe screen.
pub fn draw_session_data(frame: &mut Frame<'_>, area: Rect, state: &SessionDataState) {
    let page = ui::centered_percent_rect(area, 82, 15);
    frame.render_widget(Clear, page);
    let footer = if state.is_pending() {
        "Wipe in progress"
    } else if state.succeeded() {
        "Esc back to Sessions"
    } else if state.error().is_some() {
        "Enter/y retry  n/Esc cancel"
    } else {
        "Enter/y confirm  n/Esc cancel"
    };
    let body = ui::render_modal_frame(frame, page, "Session data", footer);

    let mut lines = vec![
        Line::from(Span::styled(
            "Delete all agent sessions",
            THEME.accent_bold(),
        )),
        Line::from(""),
        Line::from("All agent sessions and their timeline data will be deleted."),
        Line::from("Configuration, authentication, OAuth credentials, and all"),
        Line::from("non-session persistence remain untouched."),
        Line::from(""),
    ];
    if state.is_pending() {
        lines.push(ui::loading_line("Wiping session data..."));
    } else if state.succeeded() {
        lines.push(Line::from(Span::styled(
            "All agent session data has been deleted. No sessions remain.",
            THEME.success_style(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "This action cannot be undone.",
            THEME.muted_text_style(),
        )));
    }
    if let Some(error) = state.error() {
        lines.push(ui::error_line(error));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        body,
    );
}
