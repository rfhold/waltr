//! Routine catalog, launch forms, selected history, and durable operation detail.

use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use waltr_client::{
    ClientCommand, ClientManualRoutineDefinition, ClientRoutineInvocationDetail,
    ClientRoutineInvocationSummary, ClientSnapshot, RoutineRequest, RoutineRequestState,
    RoutineRequestStatus, RoutineRevision,
};

use crate::screens::sessions::command_palette::{PaletteAction, SessionCommandPalette};
use crate::{screens::SectionAction, theme::THEME, ui};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RoutineFocus {
    #[default]
    Definitions,
    History,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RoutineView {
    #[default]
    List,
    Detail,
}

/// Terminal-only focus and stable identity selections for the global screen.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RoutinesSection {
    view: RoutineView,
    focus: RoutineFocus,
    definition_index: usize,
    definition_key: Option<(String, String)>,
    history_index: usize,
    invocation_id: Option<String>,
    detail_line: usize,
    page: Option<(String, String)>,
    operations: Option<(String, String)>,
    cancel: Option<(String, String)>,
    palette: Option<SessionCommandPalette>,
    source_session: Option<String>,
    pub(crate) media: Option<crate::media::MediaEditor>,
}

impl RoutinesSection {
    pub(crate) fn open_media(&mut self, text: bool, snapshot: Option<&ClientSnapshot>) {
        self.media = if text {
            self.invocation_id.as_ref().and_then(|id| {
                snapshot?
                    .routines
                    .details
                    .get(id)?
                    .detail
                    .as_ref()
                    .map(|detail| {
                        crate::media::MediaEditor::text(
                            id.clone(),
                            detail.summary.text_results.clone(),
                        )
                    })
            })
        } else {
            Some(crate::media::MediaEditor::new(self.source_session.clone()))
        };
    }
    /// Create empty global Routine screen state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            view: RoutineView::List,
            focus: RoutineFocus::Definitions,
            definition_index: 0,
            definition_key: None,
            history_index: 0,
            invocation_id: None,
            detail_line: 0,
            page: None,
            operations: None,
            cancel: None,
            palette: None,
            source_session: None,
            media: None,
        }
    }

    /// Initialize a global or source-session route without retaining old receipts.
    pub fn begin_route(&mut self, source_session: Option<String>) -> Vec<ClientCommand> {
        let mut commands = self.release_requests();
        *self = Self::new();
        self.source_session = source_session;
        if let Some(session_id) = &self.source_session {
            commands.push(
                ClientCommand::RegisterSessionRoutineInvocationWatchInterest {
                    session_id: session_id.clone(),
                },
            );
            if let SectionAction::ClientCommands(page) = self.request_page(RoutineRevision(0)) {
                commands.extend(page);
            }
        }
        commands
    }

    /// Preserve identity selection across complete shared snapshot replacement.
    pub fn reconcile(&mut self, snapshot: &ClientSnapshot) {
        if let Some(palette) = &mut self.palette {
            palette.reconcile(snapshot);
        }
        let definitions = &snapshot.routines.definitions.definitions;
        self.definition_index = self
            .definition_key
            .as_ref()
            .and_then(|(routine, trigger)| {
                definitions.iter().position(|definition| {
                    definition.routine_name == *routine && definition.routine_revision == *trigger
                })
            })
            .unwrap_or_else(|| {
                self.definition_index
                    .min(definitions.len().saturating_sub(1))
            });
        self.definition_key = definitions.get(self.definition_index).map(|definition| {
            (
                definition.routine_name.clone(),
                definition.routine_revision.clone(),
            )
        });

        let history = self.history(snapshot);
        let selected_history_index = self
            .invocation_id
            .as_ref()
            .and_then(|id| {
                history
                    .iter()
                    .position(|summary| summary.invocation_id == *id)
            })
            .or_else(|| {
                (self.view == RoutineView::List)
                    .then(|| self.history_index.min(history.len().saturating_sub(1)))
            });
        if let Some(index) = selected_history_index {
            self.history_index = index;
            self.invocation_id = history
                .get(self.history_index)
                .map(|summary| summary.invocation_id.clone());
        }
        if self.view == RoutineView::Detail {
            self.detail_line = self.detail_line.min(
                detail_presentation(self, Some(snapshot))
                    .len()
                    .saturating_sub(1),
            );
        }
    }

    /// Focus and load one exact durable invocation, including one outside recent history.
    pub fn focus_invocation(&mut self, invocation_id: String) -> ClientCommand {
        self.view = RoutineView::Detail;
        self.invocation_id = Some(invocation_id.clone());
        self.detail_line = 0;
        ClientCommand::LoadRoutineInvocationDetail {
            request_id: next_request("routine-detail"),
            invocation_id,
        }
    }

    /// Return from invocation detail to the history list without changing watch ownership.
    pub fn dismiss_detail(&mut self, snapshot: Option<&ClientSnapshot>) -> bool {
        if self.palette.is_some() {
            return false;
        }
        if self.view != RoutineView::Detail {
            return false;
        }
        self.view = RoutineView::List;
        self.focus = RoutineFocus::History;
        self.detail_line = 0;
        self.invocation_id = snapshot
            .and_then(|snapshot| self.history(snapshot).get(self.history_index))
            .map(|summary| summary.invocation_id.clone());
        true
    }

    /// Route terminal navigation and exact manual launch input.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if let Some(snapshot) = snapshot {
            self.reconcile(snapshot);
        }
        if let Some(palette) = &mut self.palette {
            return match palette.handle_key(key, snapshot) {
                PaletteAction::Handled(action) => action,
                PaletteAction::Close(action) => {
                    self.palette = None;
                    action
                }
                PaletteAction::Navigate(id, action) => {
                    self.palette = None;
                    let mut commands = match action {
                        SectionAction::ClientCommand(c) => vec![c],
                        SectionAction::ClientCommands(c) => c,
                        _ => vec![],
                    };
                    commands.extend(self.release_detail_requests());
                    let detail = self.focus_invocation(id);
                    commands.push(detail);
                    SectionAction::ClientCommands(commands)
                }
            };
        }
        if self.view == RoutineView::Detail {
            return match key.code {
                KeyCode::Char('c') => self.request_cancel(),
                KeyCode::Char('o') => self.load_operations(snapshot),
                KeyCode::Enter => self.load_selected_detail(snapshot),
                KeyCode::Up => {
                    self.detail_line = self.detail_line.saturating_sub(1);
                    SectionAction::Continue
                }
                KeyCode::Down => {
                    self.move_detail(snapshot, 1);
                    SectionAction::Continue
                }
                KeyCode::PageUp => {
                    self.detail_line = self.detail_line.saturating_sub(10);
                    SectionAction::Continue
                }
                KeyCode::PageDown => {
                    self.move_detail(snapshot, 10);
                    SectionAction::Continue
                }
                KeyCode::Home => {
                    self.detail_line = 0;
                    SectionAction::Continue
                }
                KeyCode::End => {
                    self.detail_line = snapshot.map_or(0, |snapshot| {
                        detail_presentation(self, Some(snapshot))
                            .len()
                            .saturating_sub(1)
                    });
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            };
        }
        match key.code {
            KeyCode::Char('n') => {
                let (palette, action) =
                    SessionCommandPalette::open_context(self.source_session.clone());
                self.palette = Some(palette);
                action
            }
            KeyCode::Char('r') => self.request_page(RoutineRevision(0)),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    RoutineFocus::Definitions => RoutineFocus::History,
                    RoutineFocus::History => RoutineFocus::Definitions,
                };
                SectionAction::Continue
            }
            KeyCode::Up => self.move_selection(snapshot, false),
            KeyCode::Down => self.move_selection(snapshot, true),
            KeyCode::Enter if self.focus == RoutineFocus::History => {
                self.open_selected_detail(snapshot)
            }
            KeyCode::End | KeyCode::Char('o') if self.focus == RoutineFocus::History => {
                self.load_older(snapshot)
            }
            _ => SectionAction::Continue,
        }
    }

    fn move_selection(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
        forward: bool,
    ) -> SectionAction {
        let Some(snapshot) = snapshot else {
            return SectionAction::Continue;
        };
        match self.focus {
            RoutineFocus::Definitions => {
                let definitions = &snapshot.routines.definitions.definitions;
                self.definition_index =
                    move_index(self.definition_index, definitions.len(), forward);
                self.definition_key = definitions.get(self.definition_index).map(|definition| {
                    (
                        definition.routine_name.clone(),
                        definition.routine_revision.clone(),
                    )
                });
                SectionAction::Continue
            }
            RoutineFocus::History => {
                let history = self.history(snapshot);
                let previous = self.history_index;
                self.history_index = move_index(self.history_index, history.len(), forward);
                self.invocation_id = history
                    .get(self.history_index)
                    .map(|summary| summary.invocation_id.clone());
                if forward
                    && previous == self.history_index
                    && self.history_index + 1 >= history.len()
                {
                    self.load_older(Some(snapshot))
                } else {
                    SectionAction::Continue
                }
            }
        }
    }

    fn open_selected_detail(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        if self.invocation_id.is_none() {
            return SectionAction::Continue;
        }
        self.view = RoutineView::Detail;
        self.detail_line = 0;
        self.load_selected_detail(snapshot)
    }

    fn move_detail(&mut self, snapshot: Option<&ClientSnapshot>, amount: usize) {
        let last = detail_presentation(self, snapshot).len().saturating_sub(1);
        self.detail_line = self.detail_line.saturating_add(amount).min(last);
    }

    fn load_selected_detail(&self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(invocation_id) = self.invocation_id.as_ref() else {
            return SectionAction::Continue;
        };
        if snapshot.is_some_and(|snapshot| {
            snapshot
                .routines
                .details
                .get(invocation_id)
                .is_some_and(|detail| matches!(detail.status, "loading" | "ready"))
        }) {
            return SectionAction::Continue;
        }
        SectionAction::ClientCommand(ClientCommand::LoadRoutineInvocationDetail {
            request_id: next_request("routine-detail"),
            invocation_id: invocation_id.clone(),
        })
    }

    fn load_older(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        if self.page.is_none() {
            return self.request_page(RoutineRevision(0));
        }
        let Some(receipt) = snapshot.and_then(|s| self.page_receipt(s)) else {
            return SectionAction::Continue;
        };
        if receipt.status == RoutineRequestStatus::Expired
            || receipt.status == RoutineRequestStatus::Failed
        {
            return self.request_page(RoutineRevision(0));
        }
        if receipt.status != RoutineRequestStatus::Ready {
            return SectionAction::Continue;
        }
        let Some(cursor) = receipt.next_cursor else {
            return SectionAction::Continue;
        };
        self.request_page(cursor)
    }

    fn page_receipt<'a>(&self, snapshot: &'a ClientSnapshot) -> Option<&'a RoutineRequestState> {
        let (key, id) = self.page.as_ref()?;
        snapshot
            .routines
            .requests
            .get(key)
            .filter(|r| &r.request_id == id)
    }

    fn history<'a>(&self, snapshot: &'a ClientSnapshot) -> &'a [ClientRoutineInvocationSummary] {
        if self.page.is_some() {
            self.page_receipt(snapshot)
                .map_or(&[], |r| r.selected_invocations.as_slice())
        } else if let Some(session_id) = &self.source_session {
            snapshot
                .routines
                .session_histories
                .get(session_id)
                .map_or(&[], |h| h.recent.as_slice())
        } else {
            &snapshot.routines.history.recent
        }
    }

    fn request_page(&mut self, after_cursor: RoutineRevision) -> SectionAction {
        let mut commands = Vec::new();
        release(&mut self.page, &mut commands);
        let key = next_request("history");
        let id = next_request("page");
        self.page = Some((key.clone(), id.clone()));
        commands.push(ClientCommand::RoutineRequest {
            operation_key: key,
            request_id: id,
            command: RoutineRequest::Invocations {
                after_cursor,
                limit: 50,
                source_session: self.source_session.clone(),
            },
        });
        SectionAction::ClientCommands(commands)
    }

    fn request_cancel(&mut self) -> SectionAction {
        let Some(invocation_id) = self.invocation_id.clone() else {
            return SectionAction::Continue;
        };
        let mut commands = vec![];
        release(&mut self.cancel, &mut commands);
        let key = next_request("cancel");
        let id = next_request("cancel-request");
        self.cancel = Some((key.clone(), id.clone()));
        commands.push(ClientCommand::RoutineRequest {
            operation_key: key,
            request_id: id,
            command: RoutineRequest::Cancel { invocation_id },
        });
        SectionAction::ClientCommands(commands)
    }

    fn load_operations(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(detail) = snapshot
            .and_then(|s| s.routines.details.get(self.invocation_id.as_ref()?))
            .and_then(|d| d.detail.as_ref())
        else {
            return SectionAction::Continue;
        };
        let Some(after_cursor) = detail.next_operation_cursor else {
            return SectionAction::Continue;
        };
        let mut commands = vec![];
        release(&mut self.operations, &mut commands);
        let key = next_request("operations");
        let id = next_request("operations-request");
        self.operations = Some((key.clone(), id.clone()));
        commands.push(ClientCommand::RoutineRequest {
            operation_key: key,
            request_id: id,
            command: RoutineRequest::Operations {
                invocation_id: detail.summary.invocation_id.clone(),
                after_cursor,
                limit: 100,
            },
        });
        SectionAction::ClientCommands(commands)
    }

    /// Release exact receipt identities when leaving this route.
    pub fn release_requests(&mut self) -> Vec<ClientCommand> {
        self.media = None;
        let mut commands = vec![];
        release(&mut self.page, &mut commands);
        release(&mut self.operations, &mut commands);
        release(&mut self.cancel, &mut commands);
        if self.palette.take().is_some() {
            commands.push(ClientCommand::ReleaseRoutineDefinitionWatchInterest);
        }
        if let Some(session_id) = self.source_session.take() {
            commands
                .push(ClientCommand::ReleaseSessionRoutineInvocationWatchInterest { session_id });
        }
        commands
    }

    /// Dispose invocation-specific receipts without releasing the history page.
    pub fn release_detail_requests(&mut self) -> Vec<ClientCommand> {
        let mut commands = vec![];
        release(&mut self.operations, &mut commands);
        release(&mut self.cancel, &mut commands);
        commands
    }

    /// Whether the global parameter editor owns keyboard input.
    #[must_use]
    pub const fn editor_open(&self) -> bool {
        self.palette.is_some() || self.media.is_some()
    }

    /// Clear the protected parameter draft without changing route watch ownership.
    pub fn clear_editor(&mut self) -> bool {
        self.palette.take().is_some()
    }
}

fn release(owner: &mut Option<(String, String)>, commands: &mut Vec<ClientCommand>) {
    if let Some((operation_key, request_id)) = owner.take() {
        commands.push(ClientCommand::ReleaseRoutineRequest {
            operation_key,
            request_id,
        });
    }
}

/// Draw the Routine catalog, selected history, and exact-revision launch form.
pub fn draw_routines(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &RoutinesSection,
    snapshot: Option<&ClientSnapshot>,
) {
    if let Some(editor) = &section.media {
        editor.draw(frame, area, snapshot);
        return;
    }
    if section.view == RoutineView::Detail {
        render_detail(frame, area, section, snapshot);
        if let Some(palette) = &section.palette {
            palette.render(frame, area, snapshot);
        }
        return;
    }

    let [title, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Routines", THEME.accent_bold()),
            Span::styled(
                if section.source_session.is_some() {
                    "  session history · n new  m media  r first page  o next page"
                } else {
                    "  n manual  m media  r first page  o next page"
                },
                THEME.muted_text_style(),
            ),
        ])),
        title,
    );

    let (definitions_area, history_area) = if body.width >= 72 {
        let [definitions, _, history] = Layout::horizontal([
            Constraint::Percentage(48),
            Constraint::Length(2),
            Constraint::Percentage(52),
        ])
        .areas(body);
        (definitions, history)
    } else {
        let [definitions, _, history] = Layout::vertical([
            Constraint::Percentage(45),
            Constraint::Length(1),
            Constraint::Percentage(55),
        ])
        .areas(body);
        (definitions, history)
    };

    render_definitions(frame, definitions_area, section, snapshot);
    render_history(frame, history_area, section, snapshot);
    if let Some(palette) = &section.palette {
        palette.render(frame, area, snapshot);
    }
}

fn render_definitions(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &RoutinesSection,
    snapshot: Option<&ClientSnapshot>,
) {
    let [heading, inner] = section_areas(area);
    render_heading(
        frame,
        heading,
        "Routine catalog",
        section.focus == RoutineFocus::Definitions,
    );
    let mut lines = watch_lines(
        snapshot.map(|snapshot| &snapshot.routines.definitions.watch),
        "definitions",
    );
    if let Some(snapshot) = snapshot {
        if snapshot.routines.definitions.watch.status == "received"
            && snapshot.routines.definitions.definitions.is_empty()
        {
            lines.push(ui::loading_line("No Routines."));
        }
        let prefix_lines = lines.len();
        for (index, definition) in snapshot.routines.definitions.definitions.iter().enumerate() {
            lines.extend(definition_lines(
                definition,
                index == section.definition_index,
            ));
        }
        let selected_line = prefix_lines.saturating_add(section.definition_index.saturating_mul(2));
        let scroll = selected_wrapped_scroll(&lines, selected_line, 2, inner);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
        return;
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_history(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &RoutinesSection,
    snapshot: Option<&ClientSnapshot>,
) {
    let [heading, inner] = section_areas(area);
    render_heading(
        frame,
        heading,
        if section.page.is_some() {
            "Selected invocations"
        } else {
            "Recent invocations"
        },
        section.focus == RoutineFocus::History,
    );
    let mut lines = watch_lines(
        snapshot.map(|snapshot| &snapshot.routines.history.watch),
        "history",
    );
    if let Some(snapshot) = snapshot {
        if snapshot.routines.history.watch.status == "received"
            && section.page.is_none()
            && section.history(snapshot).is_empty()
        {
            lines.push(ui::loading_line("No Routine invocations."));
        }
        let prefix_lines = lines.len();
        for (index, invocation) in section.history(snapshot).iter().enumerate() {
            lines.extend(invocation_lines(invocation, index == section.history_index));
        }
        if let Some(receipt) = section.page_receipt(snapshot) {
            lines.push(Line::raw(format!("Selected page: {:?}", receipt.status)));
            if let Some(error) = &receipt.error {
                lines.push(ui::error_line(error));
            }
            if receipt.status == RoutineRequestStatus::Expired {
                lines.push(Line::raw("Page expired; r reloads from the first cursor."));
            }
            if receipt.next_cursor.is_some() {
                lines.push(Line::raw("o / End next page"));
            }
        } else if section.page.is_some() {
            lines.push(Line::raw("Selected page: awaiting matching receipt"));
        } else {
            lines.push(Line::raw("r / o browse authoritative pages"));
        }
        let selected_line = prefix_lines.saturating_add(section.history_index.saturating_mul(2));
        let scroll = selected_wrapped_scroll(&lines, selected_line, 2, inner);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
        return;
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &RoutinesSection,
    snapshot: Option<&ClientSnapshot>,
) {
    let [title, inner] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Routine invocation", THEME.accent_bold()),
                Span::styled("  durable operation detail", THEME.muted_text_style()),
            ]),
            Line::from(Span::styled(
                "Esc back  c request cancellation  o more operations  Up/Down scroll",
                THEME.muted_text_style(),
            )),
        ]),
        title,
    );
    let lines = detail_presentation(section, snapshot);
    let cursor = section.detail_line.min(lines.len().saturating_sub(1));
    let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let rendered_to_cursor = Paragraph::new(lines[..=cursor].to_vec())
        .wrap(Wrap { trim: false })
        .line_count(inner.width);
    let max_scroll = paragraph
        .line_count(inner.width)
        .saturating_sub(usize::from(inner.height));
    let scroll = rendered_to_cursor
        .saturating_sub(usize::from(inner.height))
        .min(max_scroll)
        .try_into()
        .unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), inner);
}

fn detail_presentation(
    section: &RoutinesSection,
    snapshot: Option<&ClientSnapshot>,
) -> Vec<Line<'static>> {
    let Some(invocation_id) = section.invocation_id.as_deref() else {
        return vec![ui::loading_line("Select an invocation.")];
    };
    let mut lines = match snapshot.and_then(|snapshot| snapshot.routines.details.get(invocation_id))
    {
        Some(detail) if detail.status == "ready" => detail.detail.as_ref().map_or_else(
            || vec![ui::loading_line("Detail unavailable.")],
            detail_lines,
        ),
        Some(detail) if detail.status == "failed" => vec![
            ui::error_line(&display_text(
                detail.error.as_deref().unwrap_or("Unable to load detail."),
            )),
            ui::loading_line("Enter retry"),
        ],
        Some(_) => vec![ui::loading_line("Loading invocation detail...")],
        None => vec![
            Line::from(Span::styled(
                format!("id {}", display_text(invocation_id)),
                THEME.muted_text_style(),
            )),
            ui::loading_line("Enter load operation detail"),
        ],
    };
    if let Some(receipt) = section.cancel.as_ref().and_then(|(key, id)| {
        snapshot?
            .routines
            .requests
            .get(key)
            .filter(|r| &r.request_id == id)
    }) {
        lines.push(Line::raw(format!(
            "Cancellation receipt: {:?}; reported status {:?} (history remains authoritative)",
            receipt.status, receipt.cancellation_status
        )));
        if let Some(error) = &receipt.error {
            lines.push(ui::error_line(error));
        }
    }
    if let Some(receipt) = section.operations.as_ref().and_then(|(key, id)| {
        snapshot?
            .routines
            .requests
            .get(key)
            .filter(|r| &r.request_id == id)
    }) {
        lines.push(Line::raw(format!("Operations page: {:?}", receipt.status)));
        if let Some(error) = &receipt.error {
            lines.push(ui::error_line(error));
        }
    }
    lines
}

fn selected_wrapped_scroll(
    lines: &[Line<'static>],
    selected_line: usize,
    selected_line_count: usize,
    area: Rect,
) -> u16 {
    let selected_line = selected_line.min(lines.len());
    let selected_end = selected_line
        .saturating_add(selected_line_count)
        .min(lines.len());
    let rendered_before = Paragraph::new(lines[..selected_line].to_vec())
        .wrap(Wrap { trim: false })
        .line_count(area.width);
    let rendered_through = Paragraph::new(lines[..selected_end].to_vec())
        .wrap(Wrap { trim: false })
        .line_count(area.width);
    rendered_through
        .saturating_sub(usize::from(area.height))
        .min(rendered_before)
        .try_into()
        .unwrap_or(u16::MAX)
}

fn section_areas(area: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area)
}

fn render_heading(frame: &mut Frame<'_>, area: Rect, title: &'static str, focused: bool) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(if focused { "> " } else { "  " }, THEME.accent_bold()),
            Span::styled(
                title,
                if focused {
                    THEME.accent_bold()
                } else {
                    THEME.muted_text_style()
                },
            ),
        ])),
        area,
    );
}

fn watch_lines(watch: Option<&waltr_client::OperationSnapshot>, noun: &str) -> Vec<Line<'static>> {
    let Some(watch) = watch else {
        return vec![ui::loading_line(format!("Loading {noun}..."))];
    };
    if watch.status == "received" {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if let Some(error) = watch.error.as_deref() {
        lines.push(ui::error_line(&display_text(error)));
    }
    lines.push(ui::loading_line(
        if matches!(watch.status, "failed" | "waiting") {
            format!("Reconnecting {noun}...")
        } else {
            format!("Loading {noun}...")
        },
    ));
    lines
}

fn definition_lines(
    definition: &ClientManualRoutineDefinition,
    selected: bool,
) -> Vec<Line<'static>> {
    let style = if selected {
        THEME.focus_style().add_modifier(Modifier::BOLD)
    } else {
        THEME.text_style()
    };
    vec![
        Line::from(vec![
            ui::selection_marker(selected),
            Span::raw(" "),
            Span::styled(display_text(&definition.routine_name), style),
        ]),
        Line::from(Span::styled(
            format!(
                "  {} @ {}: {}",
                display_text(&definition.description),
                definition.routine_revision,
                definition
                    .entrypoints
                    .iter()
                    .map(|e| format!("{} {:?} {:?}", e.name, e.trigger, e.start_contexts))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            THEME.muted_text_style(),
        )),
    ]
}

fn invocation_lines(
    invocation: &ClientRoutineInvocationSummary,
    selected: bool,
) -> Vec<Line<'static>> {
    let trigger = format!("{:?}", invocation.trigger_kind);
    let style = if selected {
        THEME.focus_style().add_modifier(Modifier::BOLD)
    } else {
        THEME.text_style()
    };
    vec![
        Line::from(vec![
            ui::selection_marker(selected),
            Span::raw(" "),
            Span::styled(display_text(&invocation.routine_name), style),
            Span::styled(
                format!(
                    "  {trigger}  {:?}  #{}",
                    invocation.status, invocation.admission_sequence.0
                ),
                THEME.muted_text_style(),
            ),
        ]),
        Line::from(Span::styled(
            format!("  {}", display_text(&invocation.invocation_id)),
            THEME.muted_text_style(),
        )),
    ]
}

fn detail_lines(detail: &ClientRoutineInvocationDetail) -> Vec<Line<'static>> {
    let summary = &detail.summary;
    let trigger = format!("{:?}", summary.trigger_kind);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(display_text(&summary.routine_name), THEME.accent_bold()),
            Span::styled(
                format!("  {trigger}  {:?}", summary.status),
                THEME.muted_text_style(),
            ),
        ]),
        Line::from(Span::styled(
            format!("id {}", display_text(&summary.invocation_id)),
            THEME.muted_text_style(),
        )),
        Line::from(Span::styled(
            format!("admission sequence {}", summary.admission_sequence.0),
            THEME.muted_text_style(),
        )),
    ];
    if let Some(session_id) = &summary.source_session {
        lines.push(Line::from(Span::styled(
            format!("source session {}", display_text(session_id)),
            THEME.muted_text_style(),
        )));
    }
    lines.push(Line::raw(format!(
        "Pinned definition {} @ {} ({}, {})",
        detail.pinned_definition.routine_name,
        detail.pinned_definition.routine_revision,
        detail.pinned_definition.api_version,
        detail.pinned_definition.engine
    )));
    lines.push(Line::raw(format!(
        "Parameters {}: {}",
        detail.pinned_definition.parameters.schema_version,
        detail.pinned_definition.parameters.schema_json.as_str()
    )));
    lines.push(Line::raw(format!(
        "Result {}: {}",
        detail.pinned_definition.result.schema_version,
        detail.pinned_definition.result.schema_json.as_str()
    )));
    lines.push(Line::raw(""));
    for operation in &detail.operations {
        lines.push(Line::raw(format!(
            "[{:?}] {} ({})",
            operation.status, operation.name, operation.id
        )));
        if let Some(result) = &operation.result {
            lines.push(Line::raw(format!("  result: {}", result.as_str())));
        }
    }
    if detail.next_operation_cursor.is_some() {
        lines.push(Line::raw("o loads more operations"));
    }
    if let Some(result) = &summary.result {
        lines.push(Line::raw(format!("Business result: {}", result.as_str())));
    }
    for helper in &summary.helpers {
        lines.push(Line::raw(format!(
            "Helper {} / {} / {:?}",
            helper.session_id, helper.definition_identity, helper.operation_id
        )));
    }
    for result in &summary.text_results {
        lines.push(Line::raw(format!(
            "Durable text {} ({}) {} UTF-8 bytes; not included in history",
            result.name, result.id, result.utf8_bytes.0
        )));
    }
    for result in &summary.binary_results {
        lines.push(Line::raw(format!(
            "Binary {} ({}, {} bytes, image {:?}); descriptor is not download authority",
            result.display_name, result.content_type, result.bytes.0, result.image
        )));
    }
    lines
}

fn move_index(current: usize, len: usize, forward: bool) -> usize {
    if forward {
        (current + 1).min(len.saturating_sub(1))
    } else {
        current.saturating_sub(1)
    }
}

fn next_request(prefix: &str) -> String {
    format!(
        "tui-{prefix}-{}",
        NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
    )
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(500)
        .collect::<String>()
        .trim()
        .to_owned()
}
