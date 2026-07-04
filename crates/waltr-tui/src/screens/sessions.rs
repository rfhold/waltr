//! Session list/detail surface: state, input, and rendering.

mod composer;
mod prompts;
mod runtime_editor;
mod template_style;
mod timeline;

use std::collections::BTreeSet;

use waltr_client::{
    ClientCommand, ClientOperationView, ClientSessionTemplateKind,
    ClientSessionTemplateSearchTarget, ClientSnapshot, ClientTimelineDirection,
    RuntimeConfigMatchView, SessionPhaseView, SessionTemplateSearchSnapshot, SessionTimelineView,
    SessionView,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph, Wrap},
};
use serde_json::{Value, json};

use crate::{
    screens::{OperationResult, OperationTracker, SectionAction},
    theme::{StatusRole, THEME},
    ui,
};

use self::{
    composer::{
        composer_cursor_at_line_column, composer_cursor_line_column, composer_line_count,
        composer_line_end, composer_line_start, composer_visible_input_line_count, render_composer,
    },
    prompts::{
        ExternalPromptModel, PromptDraft, current_external_prompt, prompt_height, render_prompt,
    },
    runtime_editor::{RuntimeEditor, RuntimeEditorKind, render_runtime_editor},
    timeline::{
        TimelineClickTarget, latest_context_tokens, shell_tool_at_row, timeline_channels,
        timeline_click_target_at_row, timeline_lines, timeline_rendered_line_count,
        timeline_widget,
    },
};

pub(crate) use self::timeline::register_timeline_watch_command;

const TIMELINE_PAGE_SIZE: u32 = 64;
const SHELL_OUTPUT_RECENT_LINE_COUNT: usize = 15;
const TIMELINE_TEXT_PREFIX: &str = "   ";
const DETAIL_COMPOSER_MAX_INPUT_LINES: u16 = 20;
const DETAIL_COMPOSER_BOTTOM_GAP: u16 = 1;
const DETAIL_RESPONSE_TOP_GAP: u16 = 1;
const DETAIL_TIMELINE_MIN_HEIGHT: u16 = 6;
const DETAIL_SCROLL_WHEEL_LINES: usize = 3;
const DETAIL_SCROLL_PAGE_LINES: usize = 12;
const RESOURCE_SEARCH_LIMIT: u32 = 12;
const PERMISSION_REQUEST_KIND: i32 = 1;
const QUESTION_REQUEST_KIND: i32 = 2;
const TIMELINE_CHANNELS: [&str; 9] = [
    "user_text",
    "assistant_text",
    "thinking",
    "tool_call",
    "tool_progress_info",
    "tool_progress_error",
    "tool_result",
    "external_request",
    "metrics",
];

/// Self-contained sessions screen state and behavior.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionsSection {
    selected_session: usize,
    detail_session_id: Option<String>,
    delete_session_id: Option<String>,
    runtime_editor: Option<RuntimeEditor>,
    pending_runtime_editor: Option<RuntimeEditorKind>,
    composer_draft: String,
    composer_cursor: usize,
    resource_autocomplete: ResourceAutocompleteState,
    timeline_scroll_from_bottom: usize,
    expanded_shell_tools: BTreeSet<String>,
    prompt_draft: PromptDraft,
    create: OperationTracker,
    delete: OperationTracker,
    runtime_update: OperationTracker,
    submit: OperationTracker,
    respond: OperationTracker,
    interrupt: OperationTracker,
    rollback: OperationTracker,
    continue_session: OperationTracker,
    rollback_modal: Option<RollbackModal>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ResourceAutocompleteState {
    request_id: Option<String>,
    session_id: Option<String>,
    query: String,
    token_start: usize,
    token_end: usize,
    selected_index: usize,
    open: bool,
    sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResourceToken {
    start: usize,
    end: usize,
    query: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RollbackModal {
    target: RollbackModalTarget,
    selected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RollbackModalTarget {
    UserMessage { cursor: u64, text: String },
    ExternalRequest { cursor: u64, label: String },
    Turn { turn: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RollbackModalAction {
    Rerun,
    Edit,
    ReAnswer,
    RollBack,
    Continue,
    Cancel,
}

impl RollbackModal {
    fn new(target: TimelineClickTarget) -> Self {
        let target = match target {
            TimelineClickTarget::UserMessage { cursor, text } => {
                RollbackModalTarget::UserMessage { cursor, text }
            }
            TimelineClickTarget::ExternalRequest { cursor, label } => {
                RollbackModalTarget::ExternalRequest { cursor, label }
            }
            TimelineClickTarget::Turn { turn } => RollbackModalTarget::Turn { turn },
        };
        Self {
            target,
            selected: 0,
        }
    }
}

fn rollback_modal_actions(target: &RollbackModalTarget) -> Vec<RollbackModalAction> {
    match target {
        RollbackModalTarget::UserMessage { .. } => vec![
            RollbackModalAction::Rerun,
            RollbackModalAction::Edit,
            RollbackModalAction::Cancel,
        ],
        RollbackModalTarget::ExternalRequest { .. } => {
            vec![RollbackModalAction::ReAnswer, RollbackModalAction::Cancel]
        }
        RollbackModalTarget::Turn { .. } => vec![
            RollbackModalAction::RollBack,
            RollbackModalAction::Continue,
            RollbackModalAction::Cancel,
        ],
    }
}

/// Status surfaced from the active session detail route to the global shell footer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDetailFooterStatus {
    /// Status text to render in the footer.
    pub text: String,
    /// Whether the status represents an error.
    pub error: bool,
    /// Whether double-Escape can interrupt the session right now.
    pub interruptible: bool,
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
            composer_draft: String::new(),
            composer_cursor: 0,
            resource_autocomplete: ResourceAutocompleteState {
                request_id: None,
                session_id: None,
                query: String::new(),
                token_start: 0,
                token_end: 0,
                selected_index: 0,
                open: false,
                sequence: 0,
            },
            timeline_scroll_from_bottom: 0,
            expanded_shell_tools: BTreeSet::new(),
            prompt_draft: PromptDraft::new(),
            create: OperationTracker::new(),
            delete: OperationTracker::new(),
            runtime_update: OperationTracker::new(),
            submit: OperationTracker::new(),
            respond: OperationTracker::new(),
            interrupt: OperationTracker::new(),
            rollback: OperationTracker::new(),
            continue_session: OperationTracker::new(),
            rollback_modal: None,
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

    /// Return the active detail status that should override the global shell footer.
    #[must_use]
    pub fn detail_footer_status(
        &self,
        snapshot: Option<&ClientSnapshot>,
    ) -> Option<SessionDetailFooterStatus> {
        let session = self.current_detail_session(snapshot)?;
        let timeline = snapshot.and_then(|snapshot| snapshot.sessions.timelines.get(&session.id));
        let prompt = current_external_prompt(snapshot, Some(session.id.as_str()));

        if let Some(status) = session_important_status(&session, timeline, prompt.as_ref()) {
            let error = timeline
                .and_then(|timeline| timeline.watch.error.as_ref())
                .is_some()
                || session.status.phase == SessionPhaseView::Failed;
            return Some(SessionDetailFooterStatus {
                text: status,
                error,
                interruptible: session_active(session.status.phase) && prompt.is_none(),
            });
        }

        None
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
            self.reset_detail_ui();
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
                self.reset_detail_ui();
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

        if let Some(Ok(_session_id)) = self.submit.reconcile(
            &snapshot.sessions.submit_user_message,
            snapshot,
            |operation| match operation {
                ClientOperationView::SessionSubmitUserMessage {
                    status,
                    error,
                    session_id,
                } => Some(match *status {
                    "ready" => OperationResult::Ready(session_id.clone()),
                    "failed" => OperationResult::Failed(
                        error
                            .as_deref()
                            .unwrap_or("session message submit failed")
                            .to_owned(),
                    ),
                    _ => OperationResult::Pending,
                }),
                _ => None,
            },
        ) {
            self.composer_draft.clear();
            self.composer_cursor = 0;
        }

        if let Some(Ok(_request_id)) = self.respond.reconcile(
            &snapshot.sessions.respond_external_request,
            snapshot,
            |operation| match operation {
                ClientOperationView::SessionRespondExternalRequest {
                    status,
                    error,
                    request_id,
                    ..
                } => Some(match *status {
                    "ready" => OperationResult::Ready(request_id.clone()),
                    "failed" => OperationResult::Failed(
                        error
                            .as_deref()
                            .unwrap_or("external request response failed")
                            .to_owned(),
                    ),
                    _ => OperationResult::Pending,
                }),
                _ => None,
            },
        ) {
            self.prompt_draft.clear();
        }

        drop(
            self.interrupt
                .reconcile(
                    &snapshot.sessions.interrupt,
                    snapshot,
                    |operation| match operation {
                        ClientOperationView::SessionInterrupt {
                            status,
                            error,
                            session_id,
                        } => Some(match *status {
                            "ready" => OperationResult::Ready(session_id.clone()),
                            "failed" => OperationResult::Failed(
                                error
                                    .as_deref()
                                    .unwrap_or("session interrupt failed")
                                    .to_owned(),
                            ),
                            _ => OperationResult::Pending,
                        }),
                        _ => None,
                    },
                ),
        );

        if let Some(Ok(_session_id)) =
            self.rollback
                .reconcile(
                    &snapshot.sessions.rollback,
                    snapshot,
                    |operation| match operation {
                        ClientOperationView::SessionRollback {
                            status,
                            error,
                            session_id,
                            ..
                        } => Some(match *status {
                            "ready" => OperationResult::Ready(session_id.clone()),
                            "failed" => OperationResult::Failed(
                                error
                                    .as_deref()
                                    .unwrap_or("session rollback failed")
                                    .to_owned(),
                            ),
                            _ => OperationResult::Pending,
                        }),
                        _ => None,
                    },
                )
        {
            self.rollback_modal = None;
        }

        if let Some(Ok(_session_id)) = self.continue_session.reconcile(
            &snapshot.sessions.continue_session,
            snapshot,
            |operation| match operation {
                ClientOperationView::SessionContinue {
                    status,
                    error,
                    session_id,
                    ..
                } => Some(match *status {
                    "ready" => OperationResult::Ready(session_id.clone()),
                    "failed" => OperationResult::Failed(
                        error
                            .as_deref()
                            .unwrap_or("session continue failed")
                            .to_owned(),
                    ),
                    _ => OperationResult::Pending,
                }),
                _ => None,
            },
        ) {
            self.rollback_modal = None;
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

    /// Dismiss the active transient overlay. Returns true when handled.
    pub fn dismiss_overlay(&mut self) -> bool {
        if self.has_pending_operation() {
            return true;
        }

        if self.runtime_editor.take().is_some() {
            self.runtime_update.clear_error();
            return true;
        }

        if self.rollback_modal.take().is_some() {
            self.rollback.clear_error();
            self.continue_session.clear_error();
            return true;
        }

        if self.pending_runtime_editor.take().is_some() {
            return true;
        }

        if self.delete_session_id.take().is_some() {
            self.delete.clear_error();
            return true;
        }

        false
    }

    /// Dismiss the active detail route or transient overlay. Returns true when handled.
    pub fn dismiss_route_or_overlay(&mut self) -> bool {
        if self.dismiss_overlay() {
            return true;
        }

        let dismissed = self.detail_session_id.take().is_some();
        if dismissed {
            self.reset_detail_ui();
        }
        dismissed
    }

    /// Clear the active detail route when a top-level route change leaves sessions.
    pub fn clear_detail_route(&mut self) -> Option<String> {
        self.runtime_editor = None;
        self.pending_runtime_editor = None;
        self.delete_session_id = None;
        let detail_session_id = self.detail_session_id.take();
        self.reset_detail_ui();
        detail_session_id
    }

    /// Start creating a session.
    pub fn create_session(&mut self) -> SectionAction {
        if self.has_pending_operation() {
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
        area: Option<Rect>,
    ) -> SectionAction {
        if self.has_pending_operation() {
            return SectionAction::Continue;
        }

        if self.runtime_editor.is_some() {
            return self.handle_runtime_editor_key(key, snapshot);
        }

        if self.rollback_modal.is_some() {
            return self.handle_rollback_modal_key(key);
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
            return self.handle_detail_key(key, snapshot, area);
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
                    if self.detail_session_id.as_deref() == Some(session.id.as_str()) {
                        return SectionAction::Continue;
                    }

                    self.detail_session_id = Some(session.id.clone());
                    return SectionAction::ClientCommand(register_timeline_watch_command(
                        session.id.clone(),
                    ));
                }
                SectionAction::Continue
            }
            _ => SectionAction::Unhandled,
        }
    }

    fn handle_detail_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
        area: Option<Rect>,
    ) -> SectionAction {
        if self.rollback_modal.is_some() {
            return self.handle_rollback_modal_key(key);
        }

        if current_external_prompt(snapshot, self.detail_session_id()).is_some() {
            return self.handle_prompt_key(key, snapshot);
        }

        match key.code {
            KeyCode::PageUp => self.page_detail_up(snapshot, area),
            KeyCode::PageDown => self.page_detail_down(snapshot, area),
            KeyCode::End => {
                self.timeline_scroll_from_bottom = 0;
                SectionAction::Continue
            }
            _ => self.handle_composer_key(key, snapshot),
        }
    }

    fn handle_composer_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        match key.code {
            KeyCode::Char('t' | 'T') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_runtime_reasoning(snapshot)
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_composer_char('\n');
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Enter => {
                if self.accept_resource_autocomplete(snapshot) {
                    SectionAction::Continue
                } else {
                    self.submit_message(snapshot)
                }
            }
            KeyCode::Esc if self.resource_autocomplete.open => {
                self.clear_resource_autocomplete();
                SectionAction::Continue
            }
            KeyCode::Backspace => {
                self.delete_composer_char_before_cursor();
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Delete => {
                self.delete_composer_char_at_cursor();
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Left => {
                self.move_composer_cursor_left();
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Right => {
                self.move_composer_cursor_right();
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Up => {
                if self.select_previous_resource_option(snapshot) {
                    return SectionAction::Continue;
                }
                self.move_composer_cursor_vertical(-1);
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Down => {
                if self.select_next_resource_option(snapshot) {
                    return SectionAction::Continue;
                }
                self.move_composer_cursor_vertical(1);
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Home => {
                self.composer_cursor =
                    composer_line_start(&self.composer_draft, self.composer_cursor);
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::End => {
                self.composer_cursor =
                    composer_line_end(&self.composer_draft, self.composer_cursor);
                self.update_resource_autocomplete(snapshot)
            }
            KeyCode::Char(character) => {
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    self.insert_composer_char(character);
                    return self.update_resource_autocomplete(snapshot);
                }
                SectionAction::Continue
            }
            _ => SectionAction::Continue,
        }
    }

    fn submit_message(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let text = self.composer_draft.trim().to_owned();
        let Some(session_id) = self.detail_session_id().map(str::to_owned) else {
            return SectionAction::Continue;
        };
        if text.is_empty() || !timeline_ready(snapshot, &session_id) {
            return SectionAction::Continue;
        }

        self.submit.begin();
        SectionAction::ClientCommand(ClientCommand::SubmitUserMessage { session_id, text })
    }

    fn update_resource_autocomplete(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(session_id) = self.detail_session_id().map(str::to_owned) else {
            self.clear_resource_autocomplete();
            return SectionAction::Continue;
        };
        if !timeline_ready(snapshot, &session_id) {
            self.clear_resource_autocomplete();
            return SectionAction::Continue;
        }
        let Some(token) = resource_token_at_cursor(&self.composer_draft, self.composer_cursor)
        else {
            self.clear_resource_autocomplete();
            return SectionAction::Continue;
        };

        if self.resource_autocomplete.open
            && self.resource_autocomplete.session_id.as_deref() == Some(session_id.as_str())
            && self.resource_autocomplete.query == token.query
            && self.resource_autocomplete.token_start == token.start
            && self.resource_autocomplete.token_end == token.end
        {
            return SectionAction::Continue;
        }

        self.resource_autocomplete.sequence += 1;
        let request_id = format!(
            "{session_id}-resource-{}",
            self.resource_autocomplete.sequence
        );
        self.resource_autocomplete.request_id = Some(request_id.clone());
        self.resource_autocomplete.session_id = Some(session_id.clone());
        self.resource_autocomplete.query = token.query.clone();
        self.resource_autocomplete.token_start = token.start;
        self.resource_autocomplete.token_end = token.end;
        self.resource_autocomplete.selected_index = 0;
        self.resource_autocomplete.open = true;

        SectionAction::ClientCommand(ClientCommand::SearchSessionTemplates {
            request_id,
            session_id,
            query: token.query,
            limit: RESOURCE_SEARCH_LIMIT,
            kinds: vec![
                ClientSessionTemplateKind::File,
                ClientSessionTemplateKind::Tool,
            ],
            target: ClientSessionTemplateSearchTarget::Resources,
        })
    }

    fn clear_resource_autocomplete(&mut self) {
        self.resource_autocomplete.request_id = None;
        self.resource_autocomplete.session_id = None;
        self.resource_autocomplete.query.clear();
        self.resource_autocomplete.token_start = 0;
        self.resource_autocomplete.token_end = 0;
        self.resource_autocomplete.selected_index = 0;
        self.resource_autocomplete.open = false;
    }

    pub(super) fn current_template_search<'a>(
        &self,
        snapshot: Option<&'a ClientSnapshot>,
    ) -> Option<&'a SessionTemplateSearchSnapshot> {
        if !self.resource_autocomplete.open {
            return None;
        }
        let session_id = self.resource_autocomplete.session_id.as_deref()?;
        let request_id = self.resource_autocomplete.request_id.as_deref()?;
        let search = snapshot?.sessions.template_searches.get(session_id)?;
        if search.request_id == request_id && search.query == self.resource_autocomplete.query {
            Some(search)
        } else {
            None
        }
    }

    pub(super) fn selected_resource_index(&self, option_count: usize) -> usize {
        self.resource_autocomplete
            .selected_index
            .min(option_count.saturating_sub(1))
    }

    fn select_previous_resource_option(&mut self, snapshot: Option<&ClientSnapshot>) -> bool {
        let Some(search) = self.current_template_search(snapshot) else {
            return false;
        };
        if search.options.is_empty() {
            return false;
        }
        self.resource_autocomplete.selected_index =
            self.resource_autocomplete.selected_index.saturating_sub(1);
        true
    }

    fn select_next_resource_option(&mut self, snapshot: Option<&ClientSnapshot>) -> bool {
        let Some(search) = self.current_template_search(snapshot) else {
            return false;
        };
        if search.options.is_empty() {
            return false;
        }
        let max = search.options.len().saturating_sub(1);
        self.resource_autocomplete.selected_index =
            (self.resource_autocomplete.selected_index + 1).min(max);
        true
    }

    fn accept_resource_autocomplete(&mut self, snapshot: Option<&ClientSnapshot>) -> bool {
        let Some(search) = self.current_template_search(snapshot) else {
            return false;
        };
        let Some(option) = search
            .options
            .get(self.selected_resource_index(search.options.len()))
        else {
            return false;
        };
        let token_start = self
            .resource_autocomplete
            .token_start
            .min(self.composer_draft.len());
        let token_end = self
            .resource_autocomplete
            .token_end
            .min(self.composer_draft.len());
        if token_start > token_end
            || !self.composer_draft.is_char_boundary(token_start)
            || !self.composer_draft.is_char_boundary(token_end)
        {
            self.clear_resource_autocomplete();
            return true;
        }
        let template = option.template.clone();
        self.composer_draft
            .replace_range(token_start..token_end, &template);
        self.composer_cursor = token_start + template.len();
        self.clear_resource_autocomplete();
        true
    }

    fn insert_composer_char(&mut self, character: char) {
        self.composer_cursor = self.composer_cursor.min(self.composer_draft.len());
        self.composer_draft.insert(self.composer_cursor, character);
        self.composer_cursor += character.len_utf8();
    }

    fn delete_composer_char_before_cursor(&mut self) {
        if self.composer_cursor == 0 {
            return;
        }
        let Some((previous, _)) = self.composer_draft[..self.composer_cursor]
            .char_indices()
            .last()
        else {
            return;
        };
        self.composer_draft.drain(previous..self.composer_cursor);
        self.composer_cursor = previous;
    }

    fn delete_composer_char_at_cursor(&mut self) {
        if self.composer_cursor >= self.composer_draft.len() {
            return;
        }
        let end = self.composer_draft[self.composer_cursor..]
            .chars()
            .next()
            .map_or(self.composer_cursor, |character| {
                self.composer_cursor + character.len_utf8()
            });
        self.composer_draft.drain(self.composer_cursor..end);
    }

    fn move_composer_cursor_left(&mut self) {
        if self.composer_cursor == 0 {
            return;
        }
        if let Some((previous, _)) = self.composer_draft[..self.composer_cursor]
            .char_indices()
            .last()
        {
            self.composer_cursor = previous;
        }
    }

    fn move_composer_cursor_right(&mut self) {
        if self.composer_cursor >= self.composer_draft.len() {
            return;
        }
        if let Some(character) = self.composer_draft[self.composer_cursor..].chars().next() {
            self.composer_cursor += character.len_utf8();
        }
    }

    fn move_composer_cursor_vertical(&mut self, direction: i8) {
        let (line, column) =
            composer_cursor_line_column(&self.composer_draft, self.composer_cursor);
        let target_line = if direction < 0 {
            line.saturating_sub(1)
        } else {
            (line + 1).min(composer_line_count(&self.composer_draft).saturating_sub(1))
        };
        if target_line == line {
            return;
        }
        self.composer_cursor =
            composer_cursor_at_line_column(&self.composer_draft, target_line, column);
    }

    fn interrupt_session(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(session) = self.current_detail_session(snapshot) else {
            return SectionAction::Continue;
        };
        if !session_active(session.status.phase) {
            return SectionAction::Continue;
        }

        self.interrupt.begin();
        SectionAction::ClientCommand(ClientCommand::InterruptSession {
            session_id: session.id,
        })
    }

    fn respond_external_request(
        &mut self,
        session_id: &str,
        request_id: &str,
        response: Value,
    ) -> SectionAction {
        self.respond.begin();
        SectionAction::ClientCommand(ClientCommand::RespondExternalRequest {
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            response_json: response.to_string(),
        })
    }

    fn deny_permission_and_interrupt(
        &mut self,
        session_id: &str,
        request_id: &str,
        message: Value,
    ) -> SectionAction {
        self.respond.begin();
        self.interrupt.begin();
        SectionAction::ClientCommands(vec![
            ClientCommand::RespondExternalRequest {
                session_id: session_id.to_owned(),
                request_id: request_id.to_owned(),
                response_json: json!({
                    "decision": "deny",
                    "message": message,
                    "mode": "cancel_work"
                })
                .to_string(),
            },
            ClientCommand::InterruptSession {
                session_id: session_id.to_owned(),
            },
        ])
    }

    fn handle_rollback_modal_key(&mut self, key: KeyEvent) -> SectionAction {
        let Some(modal) = &mut self.rollback_modal else {
            return SectionAction::Continue;
        };

        match key.code {
            KeyCode::Up => {
                modal.selected = modal.selected.saturating_sub(1);
                SectionAction::Continue
            }
            KeyCode::Down => {
                let max = rollback_modal_actions(&modal.target)
                    .len()
                    .saturating_sub(1);
                modal.selected = (modal.selected + 1).min(max);
                SectionAction::Continue
            }
            KeyCode::Esc => {
                self.rollback_modal = None;
                SectionAction::Continue
            }
            KeyCode::Enter => self.apply_selected_rollback_modal_action(),
            _ => SectionAction::Continue,
        }
    }

    fn apply_selected_rollback_modal_action(&mut self) -> SectionAction {
        let Some(modal) = self.rollback_modal.clone() else {
            return SectionAction::Continue;
        };
        let action = rollback_modal_actions(&modal.target)
            .get(modal.selected)
            .copied()
            .unwrap_or(RollbackModalAction::Cancel);
        self.apply_rollback_modal_action(&modal.target, action)
    }

    fn apply_rollback_modal_action(
        &mut self,
        target: &RollbackModalTarget,
        action: RollbackModalAction,
    ) -> SectionAction {
        let Some(session_id) = self.detail_session_id().map(str::to_owned) else {
            self.rollback_modal = None;
            return SectionAction::Continue;
        };

        match (target, action) {
            (RollbackModalTarget::UserMessage { cursor, .. }, RollbackModalAction::Rerun) => {
                self.continue_session.begin();
                self.rollback_modal = None;
                SectionAction::ClientCommand(ClientCommand::ContinueSessionFromCursor {
                    session_id,
                    cursor: *cursor,
                })
            }
            (RollbackModalTarget::UserMessage { cursor, text }, RollbackModalAction::Edit) => {
                self.rollback.begin();
                self.composer_draft = retemplatize_user_message(text);
                self.composer_cursor = self.composer_draft.len();
                self.clear_resource_autocomplete();
                self.rollback_modal = None;
                SectionAction::ClientCommand(ClientCommand::RollbackSessionBeforeCursor {
                    session_id,
                    cursor: *cursor,
                })
            }
            (
                RollbackModalTarget::ExternalRequest { cursor, .. },
                RollbackModalAction::ReAnswer,
            ) => {
                self.rollback.begin();
                self.rollback_modal = None;
                SectionAction::ClientCommand(ClientCommand::RollbackSessionToCursor {
                    session_id,
                    cursor: *cursor,
                })
            }
            (RollbackModalTarget::Turn { turn }, RollbackModalAction::RollBack) => {
                self.rollback.begin();
                self.rollback_modal = None;
                SectionAction::ClientCommand(ClientCommand::RollbackSessionToTurn {
                    session_id,
                    turn: *turn,
                })
            }
            (RollbackModalTarget::Turn { turn }, RollbackModalAction::Continue) => {
                self.continue_session.begin();
                self.rollback_modal = None;
                SectionAction::ClientCommand(ClientCommand::ContinueSessionFromTurn {
                    session_id,
                    turn: *turn,
                })
            }
            (_, RollbackModalAction::Cancel) | _ => {
                self.rollback_modal = None;
                SectionAction::Continue
            }
        }
    }

    fn page_detail_up(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
        area: Option<Rect>,
    ) -> SectionAction {
        let max_scroll = self.detail_timeline_max_scroll(snapshot, area);
        let current = self.timeline_scroll_from_bottom.min(max_scroll);
        if current >= max_scroll {
            self.timeline_scroll_from_bottom = max_scroll;
            return self.request_older_timeline(snapshot);
        }

        self.timeline_scroll_from_bottom = current
            .saturating_add(DETAIL_SCROLL_PAGE_LINES)
            .min(max_scroll);
        SectionAction::Continue
    }

    fn page_detail_down(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
        area: Option<Rect>,
    ) -> SectionAction {
        let max_scroll = self.detail_timeline_max_scroll(snapshot, area);
        let current = self.timeline_scroll_from_bottom.min(max_scroll);
        self.timeline_scroll_from_bottom = current.saturating_sub(DETAIL_SCROLL_PAGE_LINES);
        SectionAction::Continue
    }

    /// Scroll the current detail timeline toward earlier content.
    pub fn scroll_detail_up(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
        area: Rect,
    ) -> SectionAction {
        self.scroll_detail(snapshot, area, 1)
    }

    /// Scroll the current detail timeline toward later content.
    pub fn scroll_detail_down(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
        area: Rect,
    ) -> SectionAction {
        self.scroll_detail(snapshot, area, -1)
    }

    /// Apply net wheel movement to the current detail timeline.
    pub fn scroll_detail(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
        area: Rect,
        delta: i32,
    ) -> SectionAction {
        if self.detail_session_id.is_none() || delta == 0 {
            return SectionAction::Continue;
        }

        let max_scroll = self.detail_timeline_max_scroll(snapshot, Some(area));
        let current = self.timeline_scroll_from_bottom.min(max_scroll);
        let lines = DETAIL_SCROLL_WHEEL_LINES.saturating_mul(delta.unsigned_abs() as usize);
        if delta.is_positive() {
            if current >= max_scroll {
                self.timeline_scroll_from_bottom = max_scroll;
                return self.request_older_timeline(snapshot);
            }

            let next = current.saturating_add(lines);
            self.timeline_scroll_from_bottom = next.min(max_scroll);
            if next >= max_scroll {
                return self.request_older_timeline(snapshot);
            }
        } else {
            self.timeline_scroll_from_bottom = current.saturating_sub(lines);
        }

        SectionAction::Continue
    }

    /// Toggle a shell tool panel when the current detail timeline is clicked.
    pub fn handle_detail_click(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
        area: Rect,
        column: u16,
        row: u16,
    ) -> SectionAction {
        let Some(timeline_area) = self.detail_timeline_area(snapshot, area) else {
            return SectionAction::Continue;
        };
        if column < timeline_area.x
            || column >= timeline_area.x.saturating_add(timeline_area.width)
            || row < timeline_area.y
            || row >= timeline_area.y.saturating_add(timeline_area.height)
        {
            return SectionAction::Continue;
        }

        let Some(session_id) = self.detail_session_id() else {
            return SectionAction::Continue;
        };
        let Some(timeline) =
            snapshot.and_then(|snapshot| snapshot.sessions.timelines.get(session_id))
        else {
            return SectionAction::Continue;
        };
        let visible_row = usize::from(row.saturating_sub(timeline_area.y));
        let timeline_row = self
            .detail_timeline_top_scroll(Some(timeline), timeline_area)
            .saturating_add(visible_row);
        if let Some(tool_id) = shell_tool_at_row(
            timeline,
            timeline_area.width,
            timeline_row,
            &self.expanded_shell_tools,
        ) {
            if !self.expanded_shell_tools.insert(tool_id.clone()) {
                self.expanded_shell_tools.remove(&tool_id);
            }
            return SectionAction::Continue;
        }

        if let Some(target) = timeline_click_target_at_row(
            timeline,
            timeline_area.width,
            timeline_row,
            &self.expanded_shell_tools,
        ) {
            self.rollback_modal = Some(RollbackModal::new(target));
        }

        SectionAction::Continue
    }

    fn request_older_timeline(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(session_id) = self.detail_session_id().map(str::to_owned) else {
            return SectionAction::Continue;
        };
        let Some(timeline) =
            snapshot.and_then(|snapshot| snapshot.sessions.timelines.get(&session_id))
        else {
            return SectionAction::Continue;
        };
        if timeline.load.status == "loading" {
            return SectionAction::Continue;
        }
        let Some(cursor) = timeline.events.first().map(|event| event.cursor) else {
            return SectionAction::Continue;
        };

        SectionAction::ClientCommand(ClientCommand::RequestSessionTimeline {
            session_id,
            direction: ClientTimelineDirection::Backward,
            cursor: Some(cursor),
            limit: TIMELINE_PAGE_SIZE,
            channels: timeline_channels(),
        })
    }

    fn has_pending_operation(&self) -> bool {
        self.create.is_pending()
            || self.delete.is_pending()
            || self.runtime_update.is_pending()
            || self.submit.is_pending()
            || self.respond.is_pending()
            || self.interrupt.is_pending()
            || self.rollback.is_pending()
            || self.continue_session.is_pending()
    }

    fn reset_detail_ui(&mut self) {
        self.runtime_editor = None;
        self.pending_runtime_editor = None;
        self.delete_session_id = None;
        self.composer_draft.clear();
        self.composer_cursor = 0;
        self.clear_resource_autocomplete();
        self.timeline_scroll_from_bottom = 0;
        self.expanded_shell_tools.clear();
        self.prompt_draft.clear();
        self.submit.clear();
        self.respond.clear();
        self.interrupt.clear();
        self.rollback.clear();
        self.continue_session.clear();
        self.rollback_modal = None;
    }

    fn detail_timeline_area(&self, snapshot: Option<&ClientSnapshot>, area: Rect) -> Option<Rect> {
        let id = self.detail_session_id()?;
        snapshot?
            .sessions
            .current
            .iter()
            .find(|session| session.id == id)?;
        let [main_area, _error_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(status_height(snapshot, self)),
            ])
            .areas(area);
        let prompt = current_external_prompt(snapshot, Some(id));
        let response_height = self.detail_response_height(prompt.as_ref(), main_area.height);
        let [
            timeline_area,
            _response_top_gap_area,
            _response_area,
            _composer_gap_area,
        ] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(DETAIL_RESPONSE_TOP_GAP),
                Constraint::Length(response_height),
                Constraint::Length(DETAIL_COMPOSER_BOTTOM_GAP),
            ])
            .areas(main_area);

        Some(timeline_area)
    }

    fn detail_timeline_max_scroll(
        &self,
        snapshot: Option<&ClientSnapshot>,
        area: Option<Rect>,
    ) -> usize {
        let Some(timeline_area) = area.and_then(|area| self.detail_timeline_area(snapshot, area))
        else {
            return 0;
        };
        let timeline = self
            .detail_session_id()
            .and_then(|id| snapshot?.sessions.timelines.get(id));
        self.timeline_max_scroll(timeline, timeline_area)
    }

    fn detail_timeline_top_scroll(
        &self,
        timeline: Option<&SessionTimelineView>,
        timeline_area: Rect,
    ) -> usize {
        let max_scroll = self.timeline_max_scroll(timeline, timeline_area);
        max_scroll.saturating_sub(self.timeline_scroll_from_bottom.min(max_scroll))
    }

    fn timeline_max_scroll(
        &self,
        timeline: Option<&SessionTimelineView>,
        timeline_area: Rect,
    ) -> usize {
        let line_count =
            timeline_rendered_line_count(timeline, timeline_area.width, &self.expanded_shell_tools);
        line_count.saturating_sub(usize::from(timeline_area.height))
    }

    fn current_detail_session(&self, snapshot: Option<&ClientSnapshot>) -> Option<SessionView> {
        let id = self.detail_session_id()?;
        sorted_sessions_from_slice(&snapshot?.sessions.current)
            .into_iter()
            .find(|session| session.id == id)
    }

    /// Arm deletion for the selected session or active detail session.
    pub fn arm_delete(&mut self, snapshot: Option<&ClientSnapshot>) {
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

        let block = sessions_modal_block();
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let [title_area, title_gap, body_area, footer_gap, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        render_sessions_modal_title(frame, title_area);
        render_gap(frame, title_gap);

        let [header_area, content_area, status_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(status_height(snapshot, self)),
            ])
            .areas(body_area);

        frame.render_widget(header_widget(self), header_area);

        if self.in_detail() {
            frame.render_widget(detail_widget(self, snapshot), content_area);
        } else if is_pending(snapshot) {
            frame.render_widget(loading_widget(snapshot), content_area);
        } else {
            frame.render_widget(
                list_widget(self, snapshot, content_area.height),
                content_area,
            );
        }

        if status_height(snapshot, self) > 0 {
            frame.render_widget(status_widget(snapshot, self), status_area);
        }

        render_gap(frame, footer_gap);
        render_sessions_modal_footer(
            frame,
            footer_area,
            " Esc back  C-x s create  Right/Enter detail  C-x d delete ",
        );
    }

    fn render_detail(&self, frame: &mut Frame<'_>, area: Rect, snapshot: Option<&ClientSnapshot>) {
        frame.render_widget(Clear, area);

        let [main_area, error_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(status_height(snapshot, self)),
            ])
            .areas(area);

        let Some(id) = self.detail_session_id() else {
            frame.render_widget(detail_widget(self, snapshot), main_area);
            return;
        };

        let session = snapshot
            .map_or_else(Vec::new, sorted_sessions)
            .into_iter()
            .find(|session| session.id == id);

        let Some(session) = session else {
            frame.render_widget(detail_widget(self, snapshot), main_area);
            if status_height(snapshot, self) > 0 {
                frame.render_widget(status_widget(snapshot, self), error_area);
            }
            return;
        };

        let timeline = snapshot.and_then(|snapshot| snapshot.sessions.timelines.get(id));
        let prompt = current_external_prompt(snapshot, Some(id));
        let response_height = self.detail_response_height(prompt.as_ref(), main_area.height);
        let [
            timeline_area,
            _response_top_gap_area,
            response_area,
            _composer_gap_area,
        ] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(DETAIL_RESPONSE_TOP_GAP),
                Constraint::Length(response_height),
                Constraint::Length(DETAIL_COMPOSER_BOTTOM_GAP),
            ])
            .areas(main_area);

        frame.render_widget(
            timeline_widget(
                timeline,
                self.detail_timeline_top_scroll(timeline, timeline_area),
                timeline_area.width,
                &self.expanded_shell_tools,
            ),
            timeline_area,
        );

        if let Some(prompt) = &prompt {
            render_prompt(frame, response_area, prompt, &self.prompt_draft);
        } else {
            render_composer(
                frame,
                response_area,
                self,
                snapshot,
                &session,
                latest_context_tokens(timeline),
            );
        }

        if status_height(snapshot, self) > 0 {
            frame.render_widget(status_widget(snapshot, self), error_area);
        }

        if self.runtime_editor.is_some() {
            render_runtime_editor(frame, area, self, snapshot);
        }

        if self.rollback_modal.is_some() {
            render_rollback_modal(frame, area, self);
        }
    }

    fn detail_composer_height(&self) -> u16 {
        composer_visible_input_line_count(&self.composer_draft) + 4
    }

    fn detail_response_height(
        &self,
        prompt: Option<&ExternalPromptModel>,
        main_height: u16,
    ) -> u16 {
        let desired_height = prompt.map_or_else(
            || self.detail_composer_height(),
            |prompt| prompt_height(prompt, &self.prompt_draft),
        );
        let max_height = main_height
            .saturating_sub(DETAIL_TIMELINE_MIN_HEIGHT)
            .max(1);
        desired_height.min(max_height)
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
    sessions.sort_by_key(|session| std::cmp::Reverse(session_timestamp(session)));
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
            || detail_error(snapshot, section).is_some()
            || section.create.error().is_some()
            || section.delete.error().is_some()
            || section.runtime_update.error().is_some()
            || section.submit.error().is_some()
            || section.respond.error().is_some()
            || section.interrupt.error().is_some()
            || section.rollback.error().is_some()
            || section.continue_session.error().is_some(),
    )
}

fn panel_style() -> Style {
    THEME.panel_style()
}

fn panel_muted_style() -> Style {
    THEME.panel_muted_style()
}

fn sessions_modal_block() -> Block<'static> {
    Block::default()
        .style(THEME.panel_style())
        .padding(Padding::new(3, 3, 1, 1))
}

fn render_sessions_modal_title(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(sessions_modal_title_line()).style(THEME.panel_style()),
        area,
    );
}

fn sessions_modal_title_line() -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled("Sessions", THEME.accent_bold().bg(THEME.panel())),
    ])
    .style(THEME.panel_style())
}

fn render_sessions_modal_footer(frame: &mut Frame<'_>, area: Rect, footer_hint: &'static str) {
    frame.render_widget(
        Paragraph::new(sessions_modal_footer_line(footer_hint)).style(THEME.panel_style()),
        area,
    );
}

fn sessions_modal_footer_line(footer_hint: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled(footer_hint.trim().to_owned(), THEME.panel_muted_style()),
    ])
    .style(THEME.panel_style())
}

fn render_gap(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Block::default().style(THEME.panel_style()), area);
}

fn render_rollback_modal(frame: &mut Frame<'_>, area: Rect, section: &SessionsSection) {
    let Some(modal) = &section.rollback_modal else {
        return;
    };

    let modal_area = ui::centered_rect(area, 52, rollback_modal_height(modal));
    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .title(rollback_modal_title(&modal.target))
        .style(THEME.panel_style())
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    frame.render_widget(
        Paragraph::new(rollback_modal_lines(modal))
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn rollback_modal_height(modal: &RollbackModal) -> u16 {
    let action_count = rollback_modal_actions(&modal.target).len();
    u16::try_from(action_count.saturating_add(5)).unwrap_or(8)
}

fn rollback_modal_title(target: &RollbackModalTarget) -> &'static str {
    match target {
        RollbackModalTarget::UserMessage { .. } => " Message actions ",
        RollbackModalTarget::ExternalRequest { .. } => " Prompt actions ",
        RollbackModalTarget::Turn { .. } => " Turn actions ",
    }
}

fn rollback_modal_lines(modal: &RollbackModal) -> Vec<Line<'static>> {
    let mut lines = vec![rollback_modal_summary_line(&modal.target), Line::raw("")];
    lines.extend(
        rollback_modal_actions(&modal.target)
            .into_iter()
            .enumerate()
            .map(|(index, action)| rollback_modal_action_line(action, index == modal.selected)),
    );
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![Span::styled(
        "Enter select  Esc cancel",
        THEME.panel_muted_style(),
    )]));
    lines
}

fn rollback_modal_summary_line(target: &RollbackModalTarget) -> Line<'static> {
    match target {
        RollbackModalTarget::UserMessage { text, .. } => Line::from(vec![
            Span::styled("User message: ", THEME.panel_muted_style()),
            Span::styled(shorten_modal_text(text), THEME.text_style()),
        ]),
        RollbackModalTarget::ExternalRequest { label, .. } => Line::from(vec![
            Span::styled("Answered prompt: ", THEME.panel_muted_style()),
            Span::styled(label.clone(), THEME.text_style()),
        ]),
        RollbackModalTarget::Turn { turn } => Line::from(vec![
            Span::styled("Turn ", THEME.panel_muted_style()),
            Span::styled(turn.to_string(), THEME.text_style()),
        ]),
    }
}

fn rollback_modal_action_line(action: RollbackModalAction, selected: bool) -> Line<'static> {
    let style = if selected {
        THEME.focus_style().add_modifier(Modifier::BOLD)
    } else {
        THEME.text_style()
    };
    let marker = if selected { "> " } else { "  " };
    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(rollback_modal_action_label(action), style),
    ])
}

fn rollback_modal_action_label(action: RollbackModalAction) -> &'static str {
    match action {
        RollbackModalAction::Rerun => "Rerun",
        RollbackModalAction::Edit => "Edit",
        RollbackModalAction::ReAnswer => "Re-answer",
        RollbackModalAction::RollBack => "Roll back",
        RollbackModalAction::Continue => "Continue",
        RollbackModalAction::Cancel => "Cancel",
    }
}

fn shorten_modal_text(text: &str) -> String {
    const MAX: usize = 48;
    let mut chars = text.chars();
    let shortened: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        format!("{shortened}...")
    } else {
        shortened
    }
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

fn detail_error<'a>(
    snapshot: Option<&'a ClientSnapshot>,
    section: &SessionsSection,
) -> Option<&'a str> {
    let id = section.detail_session_id()?;
    snapshot.and_then(|snapshot| {
        snapshot.sessions.timelines.get(id).and_then(|timeline| {
            timeline
                .watch
                .error
                .as_deref()
                .or(timeline.load.error.as_deref())
        })
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

    Paragraph::new(Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled(title, THEME.text_style().bg(THEME.panel())),
        Span::styled("  ", THEME.panel_style()),
        Span::styled(create, THEME.focus_style().bg(THEME.panel())),
    ]))
    .style(THEME.panel_style())
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
    .style(THEME.panel_style())
}

fn list_widget(
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
    height: u16,
) -> Paragraph<'static> {
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

    let scroll_top = session_list_scroll_top(section.selected_session, sessions.len(), height);

    Paragraph::new(lines)
        .style(THEME.panel_style())
        .scroll((scroll_top, 0))
        .wrap(Wrap { trim: false })
}

fn session_list_scroll_top(selected: usize, len: usize, height: u16) -> u16 {
    if len == 0 || height == 0 {
        return 0;
    }

    let height = usize::from(height);
    let selected = selected.min(len.saturating_sub(1));
    selected
        .saturating_add(1)
        .saturating_sub(height)
        .try_into()
        .unwrap_or(u16::MAX)
}

fn session_line(section: &SessionsSection, session: &SessionView, selected: bool) -> Line<'static> {
    if section.delete_session_id.as_deref() == Some(session.id.as_str()) {
        return Line::from(vec![
            session_option_marker(selected),
            Span::styled(" ", session_option_style(selected)),
            session_option_title(session.id.clone(), selected),
            Span::styled("  ", session_option_style(selected)),
            session_delete_prompt_span(
                ui::delete_prompt_text(section.delete.is_pending(), section.delete.error()),
                selected,
            ),
        ])
        .style(session_option_style(selected));
    }

    Line::from(vec![
        session_option_marker(selected),
        Span::styled(" ", session_option_style(selected)),
        session_option_title(session.id.clone(), selected),
        Span::styled("  ", session_option_style(selected)),
        phase_span_with_bg(session.status.phase, selected),
        Span::styled("  ", session_option_style(selected)),
        Span::styled(
            format_timestamp(session),
            session_option_muted_style(selected),
        ),
        runtime_warning_span(session, selected),
    ])
    .style(session_option_style(selected))
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

    let timeline = snapshot.and_then(|snapshot| snapshot.sessions.timelines.get(id));
    let mut lines = vec![Line::from(vec![
        Span::styled("Session ", THEME.muted_text_style()),
        Span::raw(session.id),
        Span::raw("  "),
        Span::styled("status ", THEME.muted_text_style()),
        phase_span(session.status.phase),
    ])];

    if !session.status.failed_message.is_empty() {
        lines.push(ui::error_line(&session.status.failed_message));
    }

    lines.push(Line::raw(""));
    lines.extend(timeline_lines(
        timeline,
        None,
        &section.expanded_shell_tools,
    ));

    Paragraph::new(lines)
        .style(THEME.text_style())
        .wrap(Wrap { trim: false })
}

fn format_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format_suffix(value, 1_000, "k"),
        _ => format_suffix(value, 1_000_000, "m"),
    }
}

fn format_suffix(value: u64, divisor: u64, suffix: &str) -> String {
    let scaled = value as f64 / divisor as f64;
    let text = if scaled < 10.0 {
        format!("{scaled:.1}")
    } else {
        format!("{scaled:.0}")
    };
    format!("{}{suffix}", text.trim_end_matches(".0"))
}

fn cache_rate(part: u64, total: u64) -> String {
    if total == 0 {
        return "0%".to_owned();
    }
    format!("{:.1}%", (part as f64 / total as f64) * 100.0)
}

fn format_compact_cost(micros: u64) -> String {
    if micros == 0 {
        return "$-".to_owned();
    }
    format!("${:.6}", micros as f64 / 1_000_000.0)
}

fn format_compact_currency_cost(micros: u64, currency: Option<&str>) -> String {
    if micros == 0 {
        return "$-".to_owned();
    }
    let value = micros as f64 / 1_000_000.0;
    match currency {
        Some(currency) if currency != "USD" => format!("{currency} {value:.6}"),
        _ => format!("${value:.6}"),
    }
}

fn session_option_marker(selected: bool) -> Span<'static> {
    Span::styled(
        if selected { ">" } else { " " },
        if selected {
            session_option_focus_style(selected).add_modifier(Modifier::BOLD)
        } else {
            session_option_muted_style(selected)
        },
    )
}

fn session_option_title(text: impl Into<String>, selected: bool) -> Span<'static> {
    Span::styled(text.into(), session_option_title_style(selected))
}

fn session_delete_prompt_span(text: impl Into<String>, selected: bool) -> Span<'static> {
    Span::styled(
        text.into(),
        THEME
            .danger_style()
            .bg(session_option_bg(selected))
            .add_modifier(Modifier::BOLD),
    )
}

fn phase_span_with_bg(phase: SessionPhaseView, selected: bool) -> Span<'static> {
    Span::styled(
        phase_name(phase),
        THEME
            .status_style(phase_role(phase))
            .bg(session_option_bg(selected))
            .add_modifier(Modifier::BOLD),
    )
}

fn session_option_style(selected: bool) -> Style {
    THEME.panel_style().bg(session_option_bg(selected))
}

fn session_option_title_style(selected: bool) -> Style {
    if selected {
        session_option_focus_style(selected).add_modifier(Modifier::BOLD)
    } else {
        THEME
            .text_style()
            .bg(session_option_bg(selected))
            .add_modifier(Modifier::BOLD)
    }
}

fn session_option_muted_style(selected: bool) -> Style {
    THEME.panel_muted_style().bg(session_option_bg(selected))
}

fn session_option_focus_style(selected: bool) -> Style {
    THEME.focus_style().bg(session_option_bg(selected))
}

fn session_option_bg(selected: bool) -> ratatui::style::Color {
    if selected {
        THEME.dim_accent()
    } else {
        THEME.panel()
    }
}

fn runtime_warning_span(session: &SessionView, selected: bool) -> Span<'static> {
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
        Span::styled(
            "  runtime drift",
            THEME
                .status_style(StatusRole::Saving)
                .bg(session_option_bg(selected)),
        )
    } else {
        Span::raw("")
    }
}

fn status_widget(
    snapshot: Option<&ClientSnapshot>,
    section: &SessionsSection,
) -> Paragraph<'static> {
    let error = section
        .create
        .error()
        .or_else(|| section.delete.error())
        .or_else(|| section.runtime_update.error())
        .or_else(|| section.submit.error())
        .or_else(|| section.respond.error())
        .or_else(|| section.interrupt.error())
        .or_else(|| section.rollback.error())
        .or_else(|| section.continue_session.error())
        .or_else(|| detail_error(snapshot, section))
        .or_else(|| session_error(snapshot));

    let lines = error.map_or_else(Vec::new, |error| vec![ui::error_line(error)]);
    Paragraph::new(lines).style(THEME.panel_style())
}

fn phase_span(phase: SessionPhaseView) -> Span<'static> {
    Span::styled(
        phase_name(phase),
        THEME
            .status_style(phase_role(phase))
            .add_modifier(Modifier::BOLD),
    )
}

const fn phase_role(phase: SessionPhaseView) -> StatusRole {
    match phase {
        SessionPhaseView::Failed => StatusRole::Error,
        SessionPhaseView::Finished | SessionPhaseView::Idle | SessionPhaseView::Suspended => {
            StatusRole::Ready
        }
        _ => StatusRole::Loading,
    }
}

const fn phase_name(phase: SessionPhaseView) -> &'static str {
    match phase {
        SessionPhaseView::Unspecified => "unspecified",
        SessionPhaseView::Idle => "idle",
        SessionPhaseView::BuildingRequest => "building request",
        SessionPhaseView::StreamingProvider => "streaming provider",
        SessionPhaseView::RetryingProvider => "retrying provider",
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

fn session_important_status(
    session: &SessionView,
    timeline: Option<&SessionTimelineView>,
    prompt: Option<&ExternalPromptModel>,
) -> Option<String> {
    if let Some(error) = timeline.and_then(|timeline| timeline.watch.error.as_ref()) {
        return Some(error.clone());
    }

    if session.status.phase == SessionPhaseView::Failed {
        return Some(if session.status.failed_message.is_empty() {
            "Session failed.".to_owned()
        } else {
            session.status.failed_message.clone()
        });
    }

    if session_active(session.status.phase) && prompt.is_none() {
        return Some("Working...".to_owned());
    }

    None
}

const fn session_active(phase: SessionPhaseView) -> bool {
    !matches!(
        phase,
        SessionPhaseView::Unspecified
            | SessionPhaseView::Idle
            | SessionPhaseView::Suspended
            | SessionPhaseView::Finished
            | SessionPhaseView::Failed
            | SessionPhaseView::Unknown
    )
}

fn timeline_ready(snapshot: Option<&ClientSnapshot>, session_id: &str) -> bool {
    snapshot
        .and_then(|snapshot| snapshot.sessions.timelines.get(session_id))
        .is_some_and(|timeline| timeline_ready_view(Some(timeline)))
}

fn timeline_ready_view(timeline: Option<&SessionTimelineView>) -> bool {
    timeline.is_some_and(|timeline| matches!(timeline.watch.status, "opened" | "received"))
}

fn resource_token_at_cursor(text: &str, cursor: usize) -> Option<ResourceToken> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }

    let start = text[..cursor]
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            if character == '@' {
                Some(index)
            } else if character.is_whitespace() {
                Some(index + character.len_utf8())
            } else {
                None
            }
        })
        .unwrap_or(0);

    if !text[start..].starts_with('@') || cursor <= start + 1 {
        return None;
    }

    let end = text[cursor..]
        .char_indices()
        .find_map(|(offset, character)| character.is_whitespace().then_some(cursor + offset))
        .unwrap_or(text.len());
    if text[start + 1..end].chars().any(char::is_whitespace) {
        return None;
    }

    Some(ResourceToken {
        start,
        end,
        query: text[start + 1..cursor].to_owned(),
    })
}

fn retemplatize_user_message(text: &str) -> String {
    let mut output = String::new();
    let mut remainder = text;

    while let Some(start) = remainder.find("<template-result") {
        output.push_str(&remainder[..start]);
        let marker = &remainder[start..];
        let Some(open_end) = marker.find('>') else {
            output.push_str(marker);
            return output;
        };
        let open_tag = &marker[..=open_end];
        let Some(close_start) = marker.find("</template-result>") else {
            output.push_str(marker);
            return output;
        };
        let body = &marker[open_end + 1..close_start];
        if let Some(reference) = template_result_reference(open_tag, body) {
            output.push_str(&reference);
        } else {
            output.push_str(&marker[..close_start + "</template-result>".len()]);
        }
        remainder = &marker[close_start + "</template-result>".len()..];
    }

    output.push_str(remainder);
    output
}

fn template_result_reference(open_tag: &str, body: &str) -> Option<String> {
    let kind = template_attr(open_tag, "kind")?;
    match kind.as_str() {
        "file" => template_attr(open_tag, "source").map(|source| file_template_reference(&source)),
        "tool" => {
            let tool = template_attr(open_tag, "tool")?;
            let arguments = template_arguments(body).unwrap_or_else(|| "{}".to_owned());
            Some(format!("@tool({tool},{arguments})"))
        }
        _ => None,
    }
}

fn template_attr(open_tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = open_tag.find(&needle)? + needle.len();
    let end = open_tag[start..].find('"')?;
    Some(decode_xml_entities(&open_tag[start..start + end]))
}

fn template_arguments(body: &str) -> Option<String> {
    let start_tag = "<arguments>";
    let end_tag = "</arguments>";
    let start = body.find(start_tag)? + start_tag.len();
    let end = body[start..].find(end_tag)?;
    Some(decode_xml_entities(&body[start..start + end]))
}

fn file_template_reference(source: &str) -> String {
    if source
        .chars()
        .all(|character| !character.is_whitespace() && character != '"')
    {
        format!("@{source}")
    } else {
        format!("@\"{}\"", source.replace('"', "\\\""))
    }
}

fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}
