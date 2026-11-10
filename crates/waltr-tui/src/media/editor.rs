use super::{LocalPlan, LocalSelection, MediaEvent, MediaRequest};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use std::sync::atomic::{AtomicU64, Ordering};
use waltr_client::{
    ClientManualRoutineDefinition, ClientSnapshot, RoutineDirection, RoutineJson, RoutineMedia,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub(super) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MediaEditor {
    source: Option<String>,
    index: usize,
    selection: Option<(ClientManualRoutineDefinition, String)>,
    parameters: String,
    local: String,
    cursor: usize,
    local_focus: bool,
    event: Option<MediaEvent>,
    active: Option<u64>,
    text_result: Option<(String, Vec<waltr_client::RoutineTextResult>)>,
    error: Option<String>,
    scroll: u16,
}
impl std::fmt::Debug for MediaEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaEditor([private drafts])")
    }
}
impl MediaEditor {
    pub(crate) const fn new(source: Option<String>) -> Self {
        Self {
            source,
            index: 0,
            selection: None,
            parameters: String::new(),
            local: String::new(),
            cursor: 0,
            local_focus: false,
            event: None,
            active: None,
            text_result: None,
            error: None,
            scroll: 0,
        }
    }
    pub(crate) fn text(
        invocation: String,
        descriptors: Vec<waltr_client::RoutineTextResult>,
    ) -> Self {
        let mut editor = Self::new(None);
        let selected = descriptors
            .first()
            .map_or_else(String::new, |d| d.id.clone());
        editor.local = serde_json::json!({"result": selected, "destination": null}).to_string();
        editor.text_result = Some((invocation, descriptors));
        editor.local_focus = true;
        editor.cursor = editor.local.len();
        editor
    }
    pub(crate) const fn owner(&self) -> Option<u64> {
        self.active
    }
    pub(crate) fn current_selection(&self, snapshot: &ClientSnapshot) -> bool {
        self.selection
            .as_ref()
            .is_none_or(|s| self.choices(snapshot).contains(s))
    }
    pub(crate) fn receive(&mut self, event: &MediaEvent) {
        if self.active == Some(event.id) {
            self.event = Some(event.clone());
            if event.done {
                self.active = None;
            }
        }
    }
    fn choices(&self, snapshot: &ClientSnapshot) -> Vec<(ClientManualRoutineDefinition, String)> {
        if self
            .source
            .as_ref()
            .is_some_and(|source| !snapshot.sessions.current.iter().any(|s| s.id == *source))
        {
            return Vec::new();
        }
        snapshot
            .routines
            .definitions
            .definitions
            .iter()
            .flat_map(|d| {
                d.entrypoints
                    .iter()
                    .filter(|e| d.permits(&e.name, true, self.source.as_deref()))
                    .map(|e| (d.clone(), e.name.clone()))
            })
            .collect()
    }
    pub(crate) fn key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> Option<MediaRequest> {
        if key.code == KeyCode::PageDown {
            self.scroll = self.scroll.saturating_add(10);
            return None;
        }
        if key.code == KeyCode::PageUp {
            self.scroll = self.scroll.saturating_sub(10);
            return None;
        }
        if self.active.is_some() {
            if key.code == KeyCode::Char('c') {
                self.active = None;
                self.event = None;
                self.error = Some("Cancel requested; cleanup runs independently".into());
            }
            return None;
        }
        if key.code == KeyCode::Char('y')
            && let Some(event) = &self.event
        {
            let request = MediaRequest::Copy {
                id: next_id(),
                text: event.preview.clone(),
                truncated: event.truncated,
            };
            self.active = Some(request.id());
            return Some(request);
        }
        if self.selection.is_none() && self.text_result.is_none() {
            let choices = snapshot.map(|s| self.choices(s)).unwrap_or_default();
            match key.code {
                KeyCode::Down => self.index = (self.index + 1).min(choices.len().saturating_sub(1)),
                KeyCode::Up => self.index = self.index.saturating_sub(1),
                KeyCode::Enter => {
                    if let Some((definition, entrypoint)) = choices.get(self.index) {
                        self.parameters = crate::screens::sessions::command_palette::initial_json(
                            &definition.parameters.schema,
                        )
                        .to_string();
                        let plan = LocalPlan {
                            inputs: definition
                                .ports
                                .iter()
                                .filter(|p| p.required && p.direction == RoutineDirection::Input)
                                .map(|p| LocalSelection {
                                    port: p.name.clone(),
                                    path: (p.media != RoutineMedia::Text).then(String::new),
                                    text: (p.media == RoutineMedia::Text).then(String::new),
                                    mime: match p.media {
                                        RoutineMedia::File => {
                                            Some("application/octet-stream".into())
                                        }
                                        RoutineMedia::Image => Some("image/png".into()),
                                        _ => None,
                                    },
                                    pcm24k_mono: p.media == RoutineMedia::Audio,
                                })
                                .collect(),
                            saves: std::collections::BTreeMap::default(),
                            selected_text: definition
                                .ports
                                .iter()
                                .find(|p| {
                                    p.direction == RoutineDirection::Output
                                        && p.media == RoutineMedia::Text
                                })
                                .map(|p| p.name.clone()),
                        };
                        self.local = serde_json::to_string_pretty(&plan).unwrap_or_default();
                        self.cursor = self.parameters.len();
                        self.selection = Some((definition.clone(), entrypoint.clone()));
                    }
                }
                _ => {}
            }
            return None;
        }
        if key.code == KeyCode::Tab {
            self.local_focus = !self.local_focus;
            self.cursor = if self.local_focus {
                self.local.len()
            } else {
                self.parameters.len()
            };
            self.scroll = 0;
            return None;
        }
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
            return match self.submit(snapshot) {
                Ok(request) => {
                    self.active = Some(request.id());
                    self.event = None;
                    self.error = None;
                    Some(request)
                }
                Err(error) => {
                    self.error = Some(error);
                    None
                }
            };
        }
        let buffer = if self.local_focus {
            &mut self.local
        } else {
            &mut self.parameters
        };
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && buffer.len() + c.len_utf8() <= 65536 =>
            {
                buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Enter if buffer.len() < 65536 => {
                buffer.insert(self.cursor, '\n');
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let previous = buffer[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i);
                buffer.drain(previous..self.cursor);
                self.cursor = previous;
            }
            KeyCode::Delete if self.cursor < buffer.len() => {
                buffer.remove(self.cursor);
            }
            KeyCode::Left => {
                self.cursor = buffer[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i);
            }
            KeyCode::Right if self.cursor < buffer.len() => {
                self.cursor += buffer[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = buffer.len(),
            _ => {}
        }
        None
    }
    fn submit(&self, snapshot: Option<&ClientSnapshot>) -> Result<MediaRequest, String> {
        let id = next_id();
        if let Some((invocation, descriptors)) = &self.text_result {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Read {
                result: String,
                destination: Option<String>,
            }
            let read: Read = serde_json::from_str(&self.local)
                .map_err(|_| "Use result ID and explicit destination string or null".to_owned())?;
            let current = snapshot
                .and_then(|s| s.routines.details.get(invocation))
                .and_then(|d| d.detail.as_ref())
                .ok_or("Invocation detail is no longer available")?;
            if &current.summary.text_results != descriptors {
                return Err("Text descriptor selection changed; reopen this reader".into());
            }
            if !descriptors.iter().any(|d| d.id == read.result) {
                return Err("Unknown text result ID".into());
            }
            return Ok(MediaRequest::Text {
                id,
                invocation: invocation.clone(),
                descriptors: descriptors.clone(),
                selected: read.result,
                destination: read.destination,
            });
        }
        let snapshot = snapshot.ok_or("Catalog unavailable")?;
        let selected = self
            .selection
            .as_ref()
            .ok_or("Select an interactive entrypoint")?;
        if !self.choices(snapshot).contains(selected) {
            return Err("Revision, eligibility, or source changed; reopen media selection".into());
        }
        let value = RoutineJson::parse(self.parameters.clone())
            .map_err(|_| "Parameters must be complete JSON-safe values")?;
        selected
            .0
            .parameters
            .validate(&value)
            .map_err(|_| "Parameters do not match the displayed schema")?;
        let plan: LocalPlan = serde_json::from_str(&self.local)
            .map_err(|_| "Local declarations must match the displayed JSON shape")?;
        Ok(MediaRequest::Interaction {
            id,
            definition: Box::new(selected.0.clone()),
            limits: Box::new(
                snapshot
                    .routines
                    .definitions
                    .limits
                    .clone()
                    .ok_or("Server media limits unavailable")?,
            ),
            entrypoint: selected.1.clone(),
            source: self.source.clone(),
            parameters: self.parameters.clone(),
            plan: Box::new(plan),
        })
    }
    pub(crate) fn draw(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        snapshot: Option<&ClientSnapshot>,
    ) {
        frame.render_widget(Clear, area);
        let mut lines = vec![
            "Terminal media | Esc closes/cancels | c cancels active operation".to_owned(),
            "No device capture, playback, or bitmap preview. Raw PCM saves are not playback."
                .into(),
            "Tab changes JSON field; Ctrl-Enter starts; PageUp/PageDown scroll.".into(),
        ];
        if let Some((_, descriptors)) = &self.text_result {
            lines.push(
                "Durable UTF-8 reader: choose result ID; destination null previews only.".into(),
            );
            lines.extend(
                descriptors
                    .iter()
                    .map(|d| format!("{} | {} | {} UTF-8 bytes", d.id, d.name, d.utf8_bytes.0)),
            );
        } else if let Some((definition, entrypoint)) = &self.selection {
            lines.push(format!(
                "{} / {} @ {}",
                definition.routine_name, entrypoint, definition.routine_revision
            ));
            lines.push(format!(
                "Schema {}: {}",
                definition.parameters.schema_version,
                definition.parameters.schema_json.as_str()
            ));
            lines.push(format!("Ports: {:?}", definition.ports));
            lines.push("Local paths are explicit trusted selections, never RPC inputs. MIME is a hint, not validation.".into());
            lines.push("saves maps output ports to NEW local paths; no overwrite. PDF/UTF-8 remain ordinary agent inputs.".into());
        } else {
            lines.extend(
                snapshot
                    .map(|s| self.choices(s))
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                    .map(|(i, (d, e))| {
                        format!(
                            "{} {} / {} @ {}",
                            if i == self.index { ">" } else { " " },
                            d.routine_name,
                            e,
                            d.routine_revision
                        )
                    }),
            );
        }
        if self.selection.is_some() || self.text_result.is_some() {
            lines.push(
                if self.local_focus {
                    "LOCAL declarations / destinations [editing]"
                } else {
                    "BUSINESS parameters [editing]"
                }
                .into(),
            );
            let buffer = if self.local_focus {
                &self.local
            } else {
                &self.parameters
            };
            let mut display = buffer.clone();
            display.insert(self.cursor.min(display.len()), '▏');
            lines.push(display);
        }
        if let Some(error) = &self.error {
            lines.push(error.clone());
        }
        if self.active.is_some() {
            lines.push(
                "Running; Accepted is not business success. Input finishes normally; c cancels."
                    .into(),
            );
        }
        if let Some(event) = &self.event {
            lines.push(event.status.clone());
            lines.push("y copies only this displayed preview; use an explicit destination to save complete text.".into());
            if event.truncated {
                lines.push("PREVIEW TRUNCATED at 64 KiB; save to read the complete result.".into());
            }
            lines.push(event.preview.clone());
        }
        let safe = lines
            .join("\n")
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect::<String>();
        frame.render_widget(
            Paragraph::new(safe)
                .block(Block::bordered())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }
}
