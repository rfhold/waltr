//! Exact-revision manual entrypoint selection and bounded JSON parameter editing.

use crate::{screens::SectionAction, theme::THEME, ui};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};
use std::sync::atomic::{AtomicU64, Ordering};
use waltr_client::{
    ClientCommand, ClientManualRoutineDefinition, ClientSnapshot, RoutineJson, RoutineSchema,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
struct Selection {
    definition: ClientManualRoutineDefinition,
    entrypoint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Tracker {
    key: String,
    request: String,
    pending: bool,
    invocation: Option<String>,
}

/// Terminal-only draft; authoritative admission remains in the shared client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionCommandPalette {
    source: Option<String>,
    row: usize,
    search: String,
    selection: Option<Selection>,
    json: String,
    cursor: usize,
    error: Option<String>,
    tracker: Option<Tracker>,
}

impl SessionCommandPalette {
    pub(crate) fn open(source: String) -> (Self, SectionAction) {
        Self::open_context(Some(source))
    }

    pub(crate) fn open_context(source: Option<String>) -> (Self, SectionAction) {
        (
            Self {
                source,
                row: 0,
                search: String::new(),
                selection: None,
                json: String::new(),
                cursor: 0,
                error: None,
                tracker: None,
            },
            SectionAction::ClientCommand(ClientCommand::RegisterRoutineDefinitionWatchInterest),
        )
    }

    pub(crate) fn pending(&self) -> bool {
        self.tracker.as_ref().is_some_and(|t| t.pending)
    }

    pub(crate) fn release_action() -> SectionAction {
        SectionAction::ClientCommand(ClientCommand::ReleaseRoutineDefinitionWatchInterest)
    }

    fn choices(&self, snapshot: Option<&ClientSnapshot>) -> Vec<Selection> {
        let Some(snapshot) = snapshot else {
            return vec![];
        };
        // A stale session selection is not authority to start a session entrypoint.
        if self
            .source
            .as_ref()
            .is_some_and(|id| !snapshot.sessions.current.iter().any(|s| &s.id == id))
        {
            return vec![];
        }
        snapshot
            .routines
            .definitions
            .definitions
            .iter()
            .flat_map(|definition| {
                definition
                    .entrypoints
                    .iter()
                    .filter(|e| definition.permits(&e.name, false, self.source.as_deref()))
                    .map(|e| Selection {
                        definition: definition.clone(),
                        entrypoint: e.name.clone(),
                    })
            })
            .filter(|s| {
                format!(
                    "{} {} {}",
                    s.definition.routine_name, s.entrypoint, s.definition.description
                )
                .to_lowercase()
                .contains(&self.search.to_lowercase())
            })
            .collect()
    }

    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> PaletteAction {
        if self.pending() {
            return if key.code == KeyCode::Esc {
                PaletteAction::Close(Self::release_action())
            } else {
                PaletteAction::Handled(SectionAction::Continue)
            };
        }
        if let Some(id) = self.tracker.as_ref().and_then(|t| t.invocation.clone()) {
            return match key.code {
                KeyCode::Enter => PaletteAction::Navigate(id, Self::release_action()),
                KeyCode::Esc => PaletteAction::Close(Self::release_action()),
                _ => PaletteAction::Handled(SectionAction::Continue),
            };
        }
        if self.selection.is_none() {
            let choices = self.choices(snapshot);
            match key.code {
                KeyCode::Esc => return PaletteAction::Close(Self::release_action()),
                KeyCode::Up => self.row = self.row.saturating_sub(1),
                KeyCode::Down => self.row = (self.row + 1).min(choices.len().saturating_sub(1)),
                KeyCode::Enter => {
                    if let Some(selection) = choices.get(self.row) {
                        self.json =
                            initial_json(&selection.definition.parameters.schema).to_string();
                        self.cursor = self.json.len();
                        self.selection = Some(selection.clone());
                    }
                }
                KeyCode::Backspace => {
                    self.search.pop();
                    self.row = 0;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.search.push(c);
                    self.row = 0;
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => {
                    self.selection = None;
                    self.error = None;
                    self.tracker = None;
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return PaletteAction::Handled(self.submit(snapshot));
                }
                KeyCode::Tab => return PaletteAction::Handled(self.submit(snapshot)),
                KeyCode::Left => {
                    if let Some((i, _)) = self.json[..self.cursor].char_indices().last() {
                        self.cursor = i;
                    }
                }
                KeyCode::Right => {
                    if let Some(c) = self.json[self.cursor..].chars().next() {
                        self.cursor += c.len_utf8();
                    }
                }
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = self.json.len(),
                KeyCode::Backspace => {
                    if let Some((i, _)) = self.json[..self.cursor].char_indices().last() {
                        self.json.drain(i..self.cursor);
                        self.cursor = i;
                    }
                }
                KeyCode::Delete => {
                    if let Some(c) = self.json[self.cursor..].chars().next() {
                        self.json.drain(self.cursor..self.cursor + c.len_utf8());
                    }
                }
                KeyCode::Enter => self.insert('\n'),
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.insert(c)
                }
                _ => {}
            }
        }
        PaletteAction::Handled(SectionAction::Continue)
    }

    fn insert(&mut self, c: char) {
        if self.json.len() + c.len_utf8() <= 65536 {
            self.json.insert(self.cursor, c);
            self.cursor += c.len_utf8();
            self.error = None;
        }
    }

    fn submit(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(selection) = self.selection.clone() else {
            return SectionAction::Continue;
        };
        if !self.choices(snapshot).contains(&selection) {
            self.error = Some(
                "Definition, entrypoint, revision, or session authorization changed. Select again."
                    .into(),
            );
            return SectionAction::Continue;
        }
        let Ok(parameters) = RoutineJson::parse(self.json.clone()) else {
            self.invalid_parameters();
            return SectionAction::Continue;
        };
        if selection
            .definition
            .parameters
            .validate(&parameters)
            .is_err()
        {
            self.invalid_parameters();
            return SectionAction::Continue;
        }
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let key = format!("tui-start-{n}");
        let request = format!("tui-start-request-{n}");
        self.tracker = Some(Tracker {
            key: key.clone(),
            request: request.clone(),
            pending: true,
            invocation: None,
        });
        self.error = None;
        SectionAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key: key,
            request_id: request,
            routine_name: selection.definition.routine_name,
            routine_revision: selection.definition.routine_revision,
            trigger_name: selection.entrypoint,
            source_session_id: self.source.clone(),
            inputs: parameters,
        })
    }

    fn invalid_parameters(&mut self) {
        self.error = Some("Parameters must match the displayed versioned schema (bounded JSON, no duplicate keys or unsafe numbers).".into());
    }

    pub(crate) fn reconcile(&mut self, snapshot: &ClientSnapshot) {
        let Some(t) = self.tracker.as_mut().filter(|t| t.pending) else {
            return;
        };
        let Some(start) = snapshot.routines.starts.get(&t.key) else {
            return;
        };
        if start.request_id != t.request || start.source_session_id != self.source {
            return;
        }
        if start.status == "failed" {
            t.pending = false;
            self.error = Some(start.validation_failure.as_ref().map_or_else(
                || {
                    start
                        .error
                        .clone()
                        .unwrap_or_else(|| "Admission failed.".into())
                },
                |f| {
                    std::iter::once(f.message.clone())
                        .chain(
                            f.violations
                                .iter()
                                .map(|v| format!("{}: {}", v.field_path, v.message)),
                        )
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            ));
        } else if start.status == "ready" && start.invocation_id.is_some() {
            t.pending = false;
            t.invocation.clone_from(&start.invocation_id);
        }
    }

    pub(crate) fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        snapshot: Option<&ClientSnapshot>,
    ) {
        let modal = ui::centered_rect(area, area.width.min(100), area.height.min(30));
        frame.render_widget(Clear, modal);
        let body = ui::render_modal_frame(
            frame,
            modal,
            if self.source.is_some() {
                "Session commands"
            } else {
                "Global commands"
            },
            "JSON editor: Tab/Ctrl-Enter submit  Esc back",
        );
        let mut lines = vec![];
        let mut cursor_line = None;
        if let Some(id) = self.tracker.as_ref().and_then(|t| t.invocation.as_ref()) {
            lines.push(Line::raw(format!("Command accepted: {id}")));
            lines.push(Line::raw(
                "Enter inspect; acceptance is not business success.",
            ));
        } else if let Some(s) = &self.selection {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} / {} @ {}",
                    s.definition.routine_name, s.entrypoint, s.definition.routine_revision
                ),
                THEME.accent_bold(),
            )));
            lines.push(Line::raw(format!(
                "{} schema: {}",
                s.definition.parameters.schema_version,
                s.definition.parameters.schema_json.as_str()
            )));
            lines.push(Line::raw(
                "Parameters (exact JSON; file declarations are separate):",
            ));
            let prefix = &self.json[..self.cursor];
            let suffix = &self.json[self.cursor..];
            let mut edited = prefix
                .split('\n')
                .map(|s| Line::raw(s.to_owned()))
                .collect::<Vec<_>>();
            let mut rest = suffix.split('\n');
            let tail = rest.next().unwrap_or_default();
            let current = edited.last_mut().expect("split always yields one line");
            let mut chars = tail.chars();
            current.spans.push(Span::styled(
                chars.next().map_or_else(|| " ".into(), |c| c.to_string()),
                THEME.focus_style(),
            ));
            current.spans.push(Span::raw(chars.as_str().to_owned()));
            cursor_line = Some(lines.len() + edited.len());
            edited.extend(rest.map(|s| Line::raw(s.to_owned())));
            lines.extend(edited);
        } else {
            lines.push(Line::raw(format!("Search: {}", self.search)));
            for (i, s) in self.choices(snapshot).iter().enumerate() {
                lines.push(Line::raw(format!(
                    "{} {} / {} @ {}",
                    if i == self.row { ">" } else { " " },
                    s.definition.routine_name,
                    s.entrypoint,
                    s.definition.routine_revision
                )));
            }
            if self.choices(snapshot).is_empty() {
                lines.push(ui::loading_line(
                    "No authorized manual entrypoints available.",
                ));
            }
        }
        if self.pending() {
            lines.push(ui::loading_line("Awaiting admission..."));
        }
        if let Some(error) = &self.error {
            lines.push(ui::error_line(error));
        }
        let visible_through = if self.error.is_some() || self.pending() {
            lines.len()
        } else {
            cursor_line
                .unwrap_or(self.row.saturating_add(2))
                .min(lines.len())
        };
        let scroll = Paragraph::new(lines[..visible_through].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(body.width)
            .saturating_sub(usize::from(body.height))
            .try_into()
            .unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            body,
        );
    }
}

pub(crate) fn initial_json(schema: &RoutineSchema) -> serde_json::Value {
    use serde_json::Value;
    match schema {
        RoutineSchema::Null | RoutineSchema::Nullable { .. } => Value::Null,
        RoutineSchema::Boolean => false.into(),
        RoutineSchema::Number { min, .. } => Value::Number(min.clone()),
        RoutineSchema::String { .. } => "".into(),
        RoutineSchema::Array { .. } => Value::Array(vec![]),
        RoutineSchema::Object {
            properties,
            required,
        } => Value::Object(
            required
                .iter()
                .filter_map(|k| properties.get(k).map(|s| (k.clone(), initial_json(s))))
                .collect(),
        ),
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "palette returns the selected shared command"
)]
pub(crate) enum PaletteAction {
    Handled(SectionAction),
    Close(SectionAction),
    Navigate(String, SectionAction),
}

#[cfg(test)]
mod tests {
    use super::initial_json;
    use waltr_client::RoutineSchema;

    #[test]
    fn json_scaffolds_follow_every_supported_schema_type_without_fake_defaults() {
        for (schema, expected) in [
            (r#"{"type":"null"}"#, "null"),
            (r#"{"type":"nullable","schema":{"type":"boolean"}}"#, "null"),
            (r#"{"type":"boolean"}"#, "false"),
            (r#"{"type":"number","min":-2,"max":4}"#, "-2"),
            (r#"{"type":"string","maxLength":10}"#, r#""""#),
            (
                r#"{"type":"array","maxItems":2,"items":{"type":"boolean"}}"#,
                "[]",
            ),
            (
                r#"{"type":"object","properties":{"required":{"type":"boolean"},"optional":{"type":"null"}},"required":["required"]}"#,
                r#"{"required":false}"#,
            ),
        ] {
            let schema: RoutineSchema = serde_json::from_str(schema).unwrap();
            assert_eq!(initial_json(&schema).to_string(), expected);
        }
    }
}
