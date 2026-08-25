//! Dynamic terminal UI for session-scoped manual Routines.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use waltr_client::{
    ClientCommand, ClientManualRoutineDefinition, ClientRoutineInputDefinition, ClientRoutineValue,
    ClientRoutineValueType, ClientSnapshot,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{screens::SectionAction, theme::THEME, ui};

const TITLE: &str = "Session commands";
const GENERIC_FAILURE: &str = "Unable to run session command.";
static NEXT_ROUTINE_OPERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PaletteMode {
    #[default]
    List,
    Form,
    Success,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InputValueDraft {
    String { value: String, cursor: usize },
    Boolean(bool),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InputDraft {
    name: String,
    value: InputValueDraft,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InvocationTracker {
    operation_key: String,
    request_id: String,
    invocation_id: Option<String>,
    pending: bool,
}

/// Terminal-local interaction state for one open command palette.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SessionCommandPalette {
    source_session_id: String,
    mode: PaletteMode,
    search: String,
    search_cursor: usize,
    selected_row: usize,
    selected_definition: Option<ClientManualRoutineDefinition>,
    drafts: Vec<InputDraft>,
    focused_control: usize,
    field_errors: BTreeMap<String, String>,
    general_error: Option<String>,
    invocation: Option<InvocationTracker>,
}

impl SessionCommandPalette {
    pub(super) fn open(source_session_id: String) -> (Self, SectionAction) {
        (
            Self {
                source_session_id,
                mode: PaletteMode::List,
                search: String::new(),
                search_cursor: 0,
                selected_row: 0,
                selected_definition: None,
                drafts: Vec::new(),
                focused_control: 0,
                field_errors: BTreeMap::new(),
                general_error: None,
                invocation: None,
            },
            SectionAction::ClientCommands(vec![
                ClientCommand::RegisterRoutineDefinitionWatchInterest,
            ]),
        )
    }

    pub(super) fn pending(&self) -> bool {
        self.invocation
            .as_ref()
            .is_some_and(|tracker| tracker.pending)
    }

    pub(super) fn release_action() -> SectionAction {
        SectionAction::ClientCommand(ClientCommand::ReleaseRoutineDefinitionWatchInterest)
    }

    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> PaletteAction {
        if self.pending() {
            return PaletteAction::Handled(SectionAction::Continue);
        }

        match self.mode {
            PaletteMode::List => self.handle_list_key(key, snapshot),
            PaletteMode::Form => self.handle_form_key(key),
            PaletteMode::Success => match key.code {
                KeyCode::Enter => PaletteAction::Navigate(
                    self.invocation
                        .as_ref()
                        .and_then(|tracker| tracker.invocation_id.clone())
                        .expect("success mode has an accepted invocation"),
                    Self::release_action(),
                ),
                KeyCode::Esc => PaletteAction::Close(Self::release_action()),
                _ => PaletteAction::Handled(SectionAction::Continue),
            },
        }
    }

    pub(super) fn reconcile(&mut self, snapshot: &ClientSnapshot) {
        let Some(tracker) = self.invocation.as_mut().filter(|tracker| tracker.pending) else {
            return;
        };
        let Some(start) = snapshot.routines.starts.get(&tracker.operation_key) else {
            return;
        };
        if start.request_id != tracker.request_id
            || start.source_session_id != self.source_session_id
        {
            return;
        }

        if start.status == "failed" {
            tracker.pending = false;
            self.mode = PaletteMode::Form;
            self.field_errors.clear();
            if let Some(failure) = &start.validation_failure {
                let mut unmatched = Vec::new();
                for violation in &failure.violations {
                    if let Some(draft) = self.drafts.iter().find(|draft| {
                        violation.field_path == draft.name
                            || violation
                                .field_path
                                .strip_prefix("inputs.")
                                .is_some_and(|name| name == draft.name)
                    }) {
                        self.field_errors
                            .insert(draft.name.clone(), violation.message.clone());
                    } else {
                        unmatched.push(violation.message.clone());
                    }
                }
                let general = std::iter::once(failure.message.clone())
                    .chain(unmatched)
                    .filter(|message| !message.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                self.general_error = (!general.is_empty()).then_some(general);
            } else {
                self.general_error = Some(start.error.as_deref().map_or_else(
                    || GENERIC_FAILURE.to_owned(),
                    |error| format!("{GENERIC_FAILURE} {}", display_text(error)),
                ));
            }
            return;
        }

        if start.status == "ready"
            && let Some(invocation_id) = &start.invocation_id
        {
            tracker.invocation_id = Some(invocation_id.clone());
            tracker.pending = false;
            self.mode = PaletteMode::Success;
        }
    }

    pub(super) fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        snapshot: Option<&ClientSnapshot>,
    ) {
        let modal_area = ui::centered_rect(area, area.width.min(76), area.height.min(22));
        frame.render_widget(Clear, modal_area);
        let body = ui::render_modal_frame(frame, modal_area, TITLE, self.footer());
        let lines = match self.mode {
            PaletteMode::List => self.list_lines(snapshot),
            PaletteMode::Form => self.form_lines(),
            PaletteMode::Success => self.success_lines(),
        };
        frame.render_widget(
            Paragraph::new(lines)
                .style(THEME.panel_style())
                .wrap(Wrap { trim: false }),
            body,
        );
    }

    fn handle_list_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> PaletteAction {
        let visible = self.filtered_definition_indices(snapshot);
        match key.code {
            KeyCode::Esc => PaletteAction::Close(Self::release_action()),
            KeyCode::Up => {
                self.selected_row = self.selected_row.saturating_sub(1);
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Down => {
                self.selected_row = (self.selected_row + 1).min(visible.len().saturating_sub(1));
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Enter => {
                if let Some(definition) = visible
                    .get(self.selected_row)
                    .and_then(|index| {
                        self.available_definitions(snapshot)
                            .and_then(|items| items.get(*index))
                    })
                    .cloned()
                {
                    self.select_definition(&definition);
                }
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Left => {
                move_cursor_left(&self.search, &mut self.search_cursor);
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Right => {
                move_cursor_right(&self.search, &mut self.search_cursor);
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Home => {
                self.search_cursor = 0;
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::End => {
                self.search_cursor = self.search.len();
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Backspace => {
                delete_before_cursor(&mut self.search, &mut self.search_cursor);
                self.selected_row = 0;
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Delete => {
                delete_at_cursor(&mut self.search, self.search_cursor);
                self.selected_row = 0;
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                insert_at_cursor(&mut self.search, &mut self.search_cursor, character);
                self.selected_row = 0;
                PaletteAction::Handled(SectionAction::Continue)
            }
            _ => PaletteAction::Handled(SectionAction::Continue),
        }
    }

    fn handle_form_key(&mut self, key: KeyEvent) -> PaletteAction {
        let control_count = self.drafts.len() + 1;
        match key.code {
            KeyCode::Esc => {
                self.return_to_list();
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Tab => {
                self.focused_control = (self.focused_control + 1) % control_count;
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::BackTab => {
                self.focused_control = self
                    .focused_control
                    .checked_sub(1)
                    .unwrap_or(control_count - 1);
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Enter if self.focused_control == self.drafts.len() => {
                PaletteAction::Handled(self.submit())
            }
            KeyCode::Enter => {
                self.edit_focused_string('\n');
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Char(' ') => {
                if let Some(InputDraft {
                    name,
                    value: InputValueDraft::Boolean(value),
                }) = self.drafts.get_mut(self.focused_control)
                {
                    *value = !*value;
                    self.field_errors.remove(name);
                    self.general_error = None;
                } else {
                    self.edit_focused_string(' ');
                }
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Left => {
                if let Some(InputValueDraft::String { value, cursor }) = self
                    .drafts
                    .get_mut(self.focused_control)
                    .map(|draft| &mut draft.value)
                {
                    move_cursor_left(value, cursor);
                }
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Right => {
                if let Some(InputValueDraft::String { value, cursor }) = self
                    .drafts
                    .get_mut(self.focused_control)
                    .map(|draft| &mut draft.value)
                {
                    move_cursor_right(value, cursor);
                }
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Home => {
                if let Some(InputValueDraft::String { cursor, .. }) = self
                    .drafts
                    .get_mut(self.focused_control)
                    .map(|draft| &mut draft.value)
                {
                    *cursor = 0;
                }
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::End => {
                if let Some(InputValueDraft::String { value, cursor }) = self
                    .drafts
                    .get_mut(self.focused_control)
                    .map(|draft| &mut draft.value)
                {
                    *cursor = value.len();
                }
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Backspace => {
                self.edit_focused(delete_before_cursor);
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Delete => {
                self.edit_focused(|value, cursor| delete_at_cursor(value, *cursor));
                PaletteAction::Handled(SectionAction::Continue)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.edit_focused_string(character);
                PaletteAction::Handled(SectionAction::Continue)
            }
            _ => PaletteAction::Handled(SectionAction::Continue),
        }
    }

    fn edit_focused_string(&mut self, character: char) {
        self.edit_focused(|value, cursor| insert_at_cursor(value, cursor, character));
    }

    fn edit_focused(&mut self, edit: impl FnOnce(&mut String, &mut usize)) {
        let Some(draft) = self.drafts.get_mut(self.focused_control) else {
            return;
        };
        if let InputValueDraft::String { value, cursor } = &mut draft.value {
            edit(value, cursor);
            self.field_errors.remove(&draft.name);
            self.general_error = None;
        }
    }

    fn submit(&mut self) -> SectionAction {
        let Some(definition) = self.selected_definition.clone() else {
            self.general_error = Some(GENERIC_FAILURE.to_owned());
            return SectionAction::Continue;
        };
        self.field_errors.clear();
        self.general_error = None;
        for input in &definition.inputs {
            let Some(InputValueDraft::String { value, .. }) = self
                .drafts
                .iter()
                .find(|draft| draft.name == input.name)
                .map(|draft| &draft.value)
            else {
                continue;
            };
            if input.required && value.trim().is_empty() {
                self.field_errors
                    .insert(input.name.clone(), "Required.".to_owned());
            } else if let Some(minimum) = input.minimum_string_length
                && u64::try_from(value.chars().count()).unwrap_or(u64::MAX) < minimum
            {
                self.field_errors.insert(
                    input.name.clone(),
                    format!("Must contain at least {minimum} characters."),
                );
            }
        }
        if !self.field_errors.is_empty() {
            return SectionAction::Continue;
        }

        let inputs = self
            .drafts
            .iter()
            .map(|draft| {
                let value = match &draft.value {
                    InputValueDraft::String { value, .. } => {
                        ClientRoutineValue::String(value.clone())
                    }
                    InputValueDraft::Boolean(value) => ClientRoutineValue::Boolean(*value),
                };
                (draft.name.clone(), value)
            })
            .collect();
        let sequence = NEXT_ROUTINE_OPERATION.fetch_add(1, Ordering::Relaxed);
        let operation_key = format!("tui-routine-start-{sequence}");
        let request_id = format!("tui-routine-request-{sequence}");
        self.invocation = Some(InvocationTracker {
            operation_key: operation_key.clone(),
            request_id: request_id.clone(),
            invocation_id: None,
            pending: true,
        });
        SectionAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key,
            request_id,
            routine_name: definition.routine_name,
            trigger_name: definition.trigger_name,
            source_session_id: self.source_session_id.clone(),
            inputs,
        })
    }

    fn select_definition(&mut self, definition: &ClientManualRoutineDefinition) {
        self.selected_definition = Some(definition.clone());
        self.drafts = definition
            .inputs
            .iter()
            .map(|input| InputDraft {
                name: input.name.clone(),
                value: match input.r#type {
                    ClientRoutineValueType::String => InputValueDraft::String {
                        value: String::new(),
                        cursor: 0,
                    },
                    ClientRoutineValueType::Boolean => InputValueDraft::Boolean(false),
                },
            })
            .collect();
        self.focused_control = 0;
        self.field_errors.clear();
        self.general_error = None;
        self.invocation = None;
        self.mode = PaletteMode::Form;
    }

    fn return_to_list(&mut self) {
        self.mode = PaletteMode::List;
        self.selected_definition = None;
        self.drafts.clear();
        self.focused_control = 0;
        self.field_errors.clear();
        self.general_error = None;
        self.invocation = None;
    }

    fn available_definitions<'a>(
        &self,
        snapshot: Option<&'a ClientSnapshot>,
    ) -> Option<&'a [ClientManualRoutineDefinition]> {
        let definitions = &snapshot?.routines.definitions;
        (!definitions.definitions.is_empty() || definitions.watch.status == "received")
            .then_some(definitions.definitions.as_slice())
    }

    fn filtered_definition_indices(&self, snapshot: Option<&ClientSnapshot>) -> Vec<usize> {
        let query = self.search.to_lowercase();
        self.available_definitions(snapshot)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter_map(|(index, definition)| {
                (query.is_empty()
                    || definition.display_name.to_lowercase().contains(&query)
                    || definition.description.to_lowercase().contains(&query))
                .then_some(index)
            })
            .collect()
    }

    fn footer(&self) -> &'static str {
        if self.pending() {
            "Running command..."
        } else {
            match self.mode {
                PaletteMode::List => "Enter select  Esc close",
                PaletteMode::Form => "Tab next  Enter run  Esc back",
                PaletteMode::Success => "Enter navigate  Esc close",
            }
        }
    }

    fn list_lines(&self, snapshot: Option<&ClientSnapshot>) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Search commands: ", THEME.muted_text_style()),
                Span::styled(self.search.clone(), THEME.text_style()),
            ]),
            Line::raw(""),
        ];
        let watch = snapshot.map(|snapshot| &snapshot.routines.definitions.watch);
        if watch.is_none_or(|watch| watch.status != "received") {
            if let Some(error) = watch.and_then(|watch| watch.error.as_deref()) {
                lines.push(ui::error_line(&display_text(error)));
            }
            if watch.is_some_and(|watch| matches!(watch.status, "failed" | "waiting")) {
                lines.push(ui::loading_line("Reconnecting to session commands..."));
            } else {
                lines.push(ui::loading_line("Loading session commands..."));
            }
        }
        let Some(definitions) = self.available_definitions(snapshot) else {
            return lines;
        };
        let visible = self.filtered_definition_indices(snapshot);
        if visible.is_empty() {
            lines.push(ui::loading_line("No session commands available."));
            return lines;
        }
        for (row, index) in visible.into_iter().enumerate() {
            let definition = &definitions[index];
            let selected = row == self.selected_row;
            let style = if selected {
                THEME.focus_style().add_modifier(Modifier::BOLD)
            } else {
                THEME.text_style()
            };
            lines.push(Line::from(vec![
                ui::selection_marker(selected),
                Span::raw(" "),
                Span::styled(definition.display_name.clone(), style),
                Span::styled(
                    required_input_summary(&definition.inputs),
                    THEME.muted_text_style(),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!("  {}", display_text(&definition.description)),
                THEME.muted_text_style(),
            )));
        }
        lines
    }

    fn form_lines(&self) -> Vec<Line<'static>> {
        let heading = self
            .selected_definition
            .as_ref()
            .map_or("Session command", |definition| {
                definition.display_name.as_str()
            });
        let mut lines = vec![
            Line::from(Span::styled(heading.to_owned(), THEME.accent_bold())),
            Line::raw(""),
        ];
        for (index, draft) in self.drafts.iter().enumerate() {
            let value = match &draft.value {
                InputValueDraft::String { value, .. } if value.is_empty() => "<empty>".to_owned(),
                InputValueDraft::String { value, .. } => display_text(&value.replace('\n', "\\n")),
                InputValueDraft::Boolean(true) => "[x] true".to_owned(),
                InputValueDraft::Boolean(false) => "[ ] false".to_owned(),
            };
            lines.push(ui::form_field_row(
                index,
                self.focused_control,
                draft.name.clone(),
                value,
            ));
            if let Some(error) = self.field_errors.get(&draft.name) {
                lines.push(ui::error_line(&display_text(error)));
            }
        }
        lines.push(Line::raw(""));
        lines.push(ui::form_field_row(
            self.drafts.len(),
            self.focused_control,
            "Submit",
            format!("Run {heading}"),
        ));
        if let Some(error) = &self.general_error {
            lines.push(ui::error_line(&display_text(error)));
        }
        lines
    }

    fn success_lines(&self) -> Vec<Line<'static>> {
        let Some(tracker) = &self.invocation else {
            return vec![ui::error_line(GENERIC_FAILURE)];
        };
        let Some(invocation_id) = tracker.invocation_id.as_deref() else {
            return vec![ui::error_line(GENERIC_FAILURE)];
        };
        vec![
            Line::from(Span::styled("Command accepted", THEME.accent_bold())),
            Line::raw(""),
            Line::from(Span::styled(
                format!("Invocation {}", display_text(invocation_id)),
                THEME.muted_text_style(),
            )),
        ]
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "palette input returns the selected shared client command"
)]
pub(super) enum PaletteAction {
    Handled(SectionAction),
    Close(SectionAction),
    Navigate(String, SectionAction),
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

fn required_input_summary(inputs: &[ClientRoutineInputDefinition]) -> String {
    let required = inputs
        .iter()
        .filter(|input| input.required)
        .map(|input| input.name.as_str())
        .collect::<Vec<_>>();
    if required.is_empty() {
        String::new()
    } else {
        format!("  required: {}", required.join(", "))
    }
}

fn insert_at_cursor(value: &mut String, cursor: &mut usize, character: char) {
    *cursor = (*cursor).min(value.len());
    value.insert(*cursor, character);
    *cursor += character.len_utf8();
}

fn move_cursor_left(value: &str, cursor: &mut usize) {
    if *cursor > 0
        && let Some((previous, _)) = value[..*cursor].char_indices().last()
    {
        *cursor = previous;
    }
}

fn move_cursor_right(value: &str, cursor: &mut usize) {
    if let Some(character) = value[*cursor..].chars().next() {
        *cursor += character.len_utf8();
    }
}

fn delete_before_cursor(value: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let Some((previous, _)) = value[..*cursor].char_indices().last() else {
        return;
    };
    value.drain(previous..*cursor);
    *cursor = previous;
}

fn delete_at_cursor(value: &mut String, cursor: usize) {
    if cursor >= value.len() {
        return;
    }
    let end = value[cursor..]
        .chars()
        .next()
        .map_or(cursor, |character| cursor + character.len_utf8());
    value.drain(cursor..end);
}
