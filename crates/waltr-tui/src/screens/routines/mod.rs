//! Read-only global Routine definitions, history, and invocation detail.

use std::sync::atomic::{AtomicU64, Ordering};

use waltr_client::{
    ClientCommand, ClientManualRoutineDefinition, ClientRoutineInvocationDetail,
    ClientRoutineInvocationSummary, ClientRoutineTriggerKind, ClientRoutineValue, ClientSnapshot,
    ROUTINE_SOURCE_SESSION_ID_METADATA_KEY,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

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
}

impl RoutinesSection {
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
        }
    }

    /// Preserve identity selection across complete shared snapshot replacement.
    pub fn reconcile(&mut self, snapshot: &ClientSnapshot) {
        let definitions = &snapshot.routines.definitions.definitions;
        self.definition_index = self
            .definition_key
            .as_ref()
            .and_then(|(routine, trigger)| {
                definitions.iter().position(|definition| {
                    definition.routine_name == *routine && definition.trigger_name == *trigger
                })
            })
            .unwrap_or_else(|| {
                self.definition_index
                    .min(definitions.len().saturating_sub(1))
            });
        self.definition_key = definitions.get(self.definition_index).map(|definition| {
            (
                definition.routine_name.clone(),
                definition.trigger_name.clone(),
            )
        });

        let history = &snapshot.routines.history.recent;
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
            self.detail_line = self
                .detail_line
                .min(detail_line_count(snapshot, self.invocation_id.as_deref()).saturating_sub(1));
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
        if self.view != RoutineView::Detail {
            return false;
        }
        self.view = RoutineView::List;
        self.focus = RoutineFocus::History;
        self.detail_line = 0;
        self.invocation_id = snapshot
            .and_then(|snapshot| snapshot.routines.history.recent.get(self.history_index))
            .map(|summary| summary.invocation_id.clone());
        true
    }

    /// Route input without adding any global launch action.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if self.view == RoutineView::Detail {
            return match key.code {
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
                        detail_line_count(snapshot, self.invocation_id.as_deref()).saturating_sub(1)
                    });
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            };
        }
        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    RoutineFocus::Definitions => RoutineFocus::History,
                    RoutineFocus::History => RoutineFocus::Definitions,
                };
                SectionAction::Continue
            }
            KeyCode::BackTab => {
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
                        definition.trigger_name.clone(),
                    )
                });
                SectionAction::Continue
            }
            RoutineFocus::History => {
                let history = &snapshot.routines.history.recent;
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
        let last = snapshot.map_or(0, |snapshot| {
            detail_line_count(snapshot, self.invocation_id.as_deref()).saturating_sub(1)
        });
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

    fn load_older(&self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(history) = snapshot.map(|snapshot| &snapshot.routines.history) else {
            return SectionAction::Continue;
        };
        if history.older_page_token.is_none() || history.pagination.status == "loading" {
            return SectionAction::Continue;
        }
        SectionAction::ClientCommand(ClientCommand::LoadOlderRoutineInvocations {
            request_id: next_request("routine-history"),
            page_size: 50,
        })
    }
}

/// Draw the complete read-only global Routine screen.
pub fn draw_routines(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &RoutinesSection,
    snapshot: Option<&ClientSnapshot>,
) {
    if section.view == RoutineView::Detail {
        render_detail(frame, area, section, snapshot);
        return;
    }

    let [title, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Routines", THEME.accent_bold()),
            Span::styled(
                "  read-only definitions and durable history",
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
        "Manual definitions",
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
            lines.push(ui::loading_line("No manual Routines."));
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
        "Recent invocations",
        section.focus == RoutineFocus::History,
    );
    let mut lines = watch_lines(
        snapshot.map(|snapshot| &snapshot.routines.history.watch),
        "history",
    );
    if let Some(snapshot) = snapshot {
        if snapshot.routines.history.watch.status == "received"
            && snapshot.routines.history.recent.is_empty()
        {
            lines.push(ui::loading_line("No Routine invocations."));
        }
        let prefix_lines = lines.len();
        for (index, invocation) in snapshot.routines.history.recent.iter().enumerate() {
            lines.extend(invocation_lines(invocation, index == section.history_index));
        }
        if snapshot.routines.history.pagination.status == "loading" {
            lines.push(ui::loading_line("Loading older invocations..."));
        } else if let Some(error) = snapshot.routines.history.pagination.error.as_deref() {
            lines.push(ui::error_line(&display_text(error)));
            lines.push(ui::loading_line("o retry older history"));
        } else if snapshot.routines.history.older_page_token.is_some() {
            lines.push(ui::loading_line("o / End  load older"));
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
                Span::styled("  read-only durable detail", THEME.muted_text_style()),
            ]),
            Line::from(Span::styled(
                "Esc back  Up/Down scroll  PgUp/PgDn page  Home/End",
                THEME.muted_text_style(),
            )),
        ]),
        title,
    );
    let Some(invocation_id) = section.invocation_id.as_deref() else {
        frame.render_widget(
            Paragraph::new(ui::loading_line("Select an invocation.")),
            inner,
        );
        return;
    };
    let lines = match snapshot.and_then(|snapshot| snapshot.routines.details.get(invocation_id)) {
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
            ui::loading_line("Enter load task detail"),
        ],
    };
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

fn detail_line_count(snapshot: &ClientSnapshot, invocation_id: Option<&str>) -> usize {
    invocation_id
        .and_then(|invocation_id| snapshot.routines.details.get(invocation_id))
        .and_then(|detail| detail.detail.as_ref())
        .map_or(1, |detail| detail_lines(detail).len())
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
            Span::styled(display_text(&definition.display_name), style),
        ]),
        Line::from(Span::styled(
            format!("  {}", display_text(&definition.description)),
            THEME.muted_text_style(),
        )),
    ]
}

fn invocation_lines(
    invocation: &ClientRoutineInvocationSummary,
    selected: bool,
) -> Vec<Line<'static>> {
    let trigger = match invocation.trigger_kind {
        ClientRoutineTriggerKind::Manual => "manual",
        ClientRoutineTriggerKind::Event => "event",
    };
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
                    invocation.status, invocation.admission_sequence
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
    let trigger = match summary.trigger_kind {
        ClientRoutineTriggerKind::Manual => "manual",
        ClientRoutineTriggerKind::Event => "event",
    };
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
            format!("admission sequence {}", summary.admission_sequence),
            THEME.muted_text_style(),
        )),
    ];
    if let Some(session_id) = summary.metadata.get(ROUTINE_SOURCE_SESSION_ID_METADATA_KEY) {
        lines.push(Line::from(Span::styled(
            format!("source session {}", display_text(session_id)),
            THEME.muted_text_style(),
        )));
    }
    if let Some(cursor) = summary.source_cursor.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("source cursor {}", display_text(cursor)),
            THEME.muted_text_style(),
        )));
    }
    if let Some(error) = &summary.terminal_error {
        lines.push(ui::error_line(&display_text(&error.message)));
    }
    lines.push(Line::raw(""));
    for task in &detail.tasks {
        lines.push(Line::from(vec![
            Span::styled(format!("[{:?}] ", task.status), THEME.muted_text_style()),
            Span::styled(display_text(&task.name), THEME.text_style()),
            Span::styled(
                format!("  {}", display_text(&task.r#type)),
                THEME.muted_text_style(),
            ),
        ]));
        if let Some(error) = &task.error {
            lines.push(ui::error_line(&display_text(&error.message)));
        }
        for result in &task.results {
            lines.push(result_line("  ", result));
        }
        for (name, session_id) in &task.session_references {
            lines.push(Line::from(Span::styled(
                format!("  {}: {}", display_text(name), display_text(session_id)),
                THEME.muted_text_style(),
            )));
        }
    }
    if !detail.results.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("Results", THEME.accent_bold())));
        for result in &detail.results {
            lines.push(result_line("", result));
        }
    }
    lines
}

fn result_line(prefix: &str, result: &waltr_client::ClientRoutineResult) -> Line<'static> {
    let value = match &result.value {
        ClientRoutineValue::String(value) => display_text(value),
        ClientRoutineValue::Boolean(value) => value.to_string(),
    };
    Line::from(vec![
        Span::styled(
            format!("{prefix}{}: ", display_text(&result.name)),
            THEME.muted_text_style(),
        ),
        Span::styled(value, THEME.text_style()),
    ])
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
