//! Inference settings editor for session detail routes.

use waltr_client::{ClientCommand, ClientSnapshot, ConfigView, ModelView, SessionView};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};

use crate::{
    screens::{
        SectionAction,
        inference_selector::{self, ModelSelectorEntry},
    },
    theme::THEME,
    ui,
};

use super::SessionsSection;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InferenceEditor {
    query: String,
    query_cursor: usize,
    selected_model: usize,
}

impl SessionsSection {
    fn open_inference_editor(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        if config(snapshot).is_none() {
            self.pending_inference_editor = true;
            return SectionAction::ClientCommand(ClientCommand::RequestConfig);
        }

        self.open_provider_editor(snapshot);

        SectionAction::Continue
    }

    /// Open provider/model selection for the active detail session.
    pub fn open_inference_settings_editor(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if self.detail_session_id.is_none() {
            return SectionAction::Continue;
        }

        self.open_inference_editor(snapshot)
    }

    /// Interrupt the active detail session when it is active.
    pub fn interrupt_current_session(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if self.detail_session_id.is_none() {
            return SectionAction::Continue;
        }

        self.interrupt_session(snapshot)
    }

    pub(super) fn handle_inference_editor_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        let Some(editor) = &mut self.inference_editor else {
            return SectionAction::Unhandled;
        };

        let action = handle_inference_editor_key(
            snapshot,
            key,
            &mut editor.query,
            &mut editor.query_cursor,
            &mut editor.selected_model,
        );

        match action {
            InferenceEditorKeyAction::Continue => SectionAction::Continue,
            InferenceEditorKeyAction::Close => {
                self.inference_editor = None;
                SectionAction::Continue
            }
            InferenceEditorKeyAction::Save => self.save_inference_settings(snapshot),
        }
    }

    pub(super) fn open_provider_editor(&mut self, snapshot: Option<&ClientSnapshot>) {
        if filtered_model_entries(snapshot, "").is_empty() {
            return;
        }
        self.inference_settings_update.clear_error();
        self.inference_editor = Some(InferenceEditor {
            query: String::new(),
            query_cursor: 0,
            selected_model: 0,
        });
    }

    pub(super) fn cycle_inference_reasoning(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if config(snapshot).is_none() {
            return SectionAction::ClientCommand(ClientCommand::RequestConfig);
        }
        let Some(session) = self.current_detail_session(snapshot) else {
            return SectionAction::Continue;
        };
        let Some(provider) = session
            .inference_settings
            .as_ref()
            .and_then(|settings| settings.provider.as_ref())
        else {
            return SectionAction::Continue;
        };
        let Some(entry) = filtered_model_entries(snapshot, "")
            .into_iter()
            .find(|entry| {
                (entry.provider_key.as_str(), entry.model_id.as_str())
                    == (provider.key.as_str(), provider.model_id.as_str())
            })
        else {
            return SectionAction::Continue;
        };
        let efforts = reasoning_efforts_for_model(&entry.model);
        if efforts.len() <= 1 {
            return SectionAction::Continue;
        }
        let current = provider.reasoning_effort.as_deref().unwrap_or("");
        let current_index = efforts
            .iter()
            .position(|effort| effort == current)
            .unwrap_or(0);
        let next_effort = &efforts[(current_index + 1) % efforts.len()];
        let reasoning_effort = (!next_effort.is_empty()).then(|| next_effort.clone());
        self.inference_settings_update.begin();
        SectionAction::ClientCommand(ClientCommand::UpdateSessionInferenceSettings {
            id: session.id,
            provider_key: provider.key.clone(),
            model_id: provider.model_id.clone(),
            reasoning_effort,
        })
    }

    fn save_inference_settings(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(session) = self.current_detail_session(snapshot) else {
            return SectionAction::Continue;
        };
        let Some(InferenceEditor {
            query,
            selected_model,
            ..
        }) = &self.inference_editor
        else {
            return SectionAction::Continue;
        };
        let Some(entry) = filtered_model_entries(snapshot, query)
            .get(*selected_model)
            .cloned()
        else {
            return SectionAction::Continue;
        };
        let reasoning_effort = session
            .inference_settings
            .as_ref()
            .and_then(|settings| settings.provider.as_ref())
            .and_then(|provider| provider.reasoning_effort.clone())
            .filter(|effort| entry.model.options.reasoning_efforts.contains(effort));
        self.inference_settings_update.begin();
        SectionAction::ClientCommand(ClientCommand::UpdateSessionInferenceSettings {
            id: session.id,
            provider_key: entry.provider_key,
            model_id: entry.model_id,
            reasoning_effort,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InferenceEditorKeyAction {
    Continue,
    Close,
    Save,
}

fn handle_inference_editor_key(
    snapshot: Option<&ClientSnapshot>,
    key: KeyEvent,
    query: &mut String,
    query_cursor: &mut usize,
    selected_model: &mut usize,
) -> InferenceEditorKeyAction {
    match key.code {
        KeyCode::Esc => InferenceEditorKeyAction::Close,
        KeyCode::Up => {
            *selected_model = selected_model.saturating_sub(1);
            InferenceEditorKeyAction::Continue
        }
        KeyCode::Down => {
            let max = filtered_model_entries(snapshot, query)
                .len()
                .saturating_sub(1);
            *selected_model = (*selected_model + 1).min(max);
            InferenceEditorKeyAction::Continue
        }
        KeyCode::Enter => InferenceEditorKeyAction::Save,
        _ => {
            if handle_search_key(query, query_cursor, key) == SearchKeyAction::TextChanged {
                *selected_model = 0;
            }
            InferenceEditorKeyAction::Continue
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchKeyAction {
    TextChanged,
    CursorMoved,
    Unhandled,
}

fn handle_search_key(query: &mut String, cursor: &mut usize, key: KeyEvent) -> SearchKeyAction {
    *cursor = (*cursor).min(char_count(query));
    match key.code {
        KeyCode::Left => {
            *cursor = cursor.saturating_sub(1);
            SearchKeyAction::CursorMoved
        }
        KeyCode::Right => {
            *cursor = (*cursor + 1).min(char_count(query));
            SearchKeyAction::CursorMoved
        }
        KeyCode::Home => {
            *cursor = 0;
            SearchKeyAction::CursorMoved
        }
        KeyCode::End => {
            *cursor = char_count(query);
            SearchKeyAction::CursorMoved
        }
        KeyCode::Backspace => {
            if *cursor == 0 {
                return SearchKeyAction::CursorMoved;
            }
            let start = byte_index_at_char(query, cursor.saturating_sub(1));
            let end = byte_index_at_char(query, *cursor);
            query.replace_range(start..end, "");
            *cursor -= 1;
            SearchKeyAction::TextChanged
        }
        KeyCode::Delete => {
            if *cursor >= char_count(query) {
                return SearchKeyAction::CursorMoved;
            }
            let start = byte_index_at_char(query, *cursor);
            let end = byte_index_at_char(query, *cursor + 1);
            query.replace_range(start..end, "");
            SearchKeyAction::TextChanged
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            let index = byte_index_at_char(query, *cursor);
            query.insert(index, character);
            *cursor += 1;
            SearchKeyAction::TextChanged
        }
        _ => SearchKeyAction::Unhandled,
    }
}

fn char_count(value: &str) -> usize {
    value.chars().count()
}

fn byte_index_at_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map_or(value.len(), |(index, _)| index)
}

pub(super) fn render_inference_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) {
    let modal_area = ui::centered_percent_rect(area, 82, 42);
    frame.render_widget(Clear, area);
    frame.render_widget(Clear, modal_area);

    if section.inference_editor.is_none() {
        return;
    }
    let title = " Inference settings ";
    let footer = " Esc cancel  type search  Enter save ";
    let editor_area = ui::render_modal_frame(frame, modal_area, title, footer);

    let session = section.current_detail_session(snapshot);
    if let Some(InferenceEditor {
        query,
        query_cursor,
        selected_model,
    }) = &section.inference_editor
    {
        render_provider_editor(
            frame,
            editor_area,
            snapshot,
            session.as_ref(),
            query,
            *query_cursor,
            *selected_model,
        );
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "render helper receives independent provider editor inputs"
)]
fn render_provider_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&ClientSnapshot>,
    session: Option<&SessionView>,
    query: &str,
    query_cursor: usize,
    selected_model: usize,
) {
    let entries = filtered_model_entries(snapshot, query);

    inference_selector::fill_panel(frame, area);
    let [note_area, note_gap, search_area, search_gap, body_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    render_note_panel(
        frame,
        note_area,
        &["Select provider/model for the next prompt."],
    );
    ui::render_panel_gap(frame, note_gap);
    render_search_panel(frame, search_area, "Search", query, query_cursor);
    ui::render_panel_gap(frame, search_gap);

    let selector_entries = model_selector_entries(&entries, selected_model, session);
    inference_selector::render_model_selector(frame, body_area, "Models", &selector_entries);
}

fn model_selector_entries(
    entries: &[ProviderModelEntry],
    selected_model: usize,
    session: Option<&SessionView>,
) -> Vec<ModelSelectorEntry> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let focused = index == selected_model;
            let current = session
                .and_then(|session| session.inference_settings.as_ref())
                .and_then(|settings| settings.provider.as_ref())
                .is_some_and(|runtime_provider| {
                    (
                        runtime_provider.key.as_str(),
                        runtime_provider.model_id.as_str(),
                    ) == (entry.provider_key.as_str(), entry.model_id.as_str())
                });
            ModelSelectorEntry {
                provider_key: entry.provider_key.clone(),
                model_id: entry.model_id.clone(),
                model_name: entry.model_name.clone(),
                focused,
                current_badge: current.then_some("current"),
            }
        })
        .collect()
}

fn render_note_panel(frame: &mut Frame<'_>, area: Rect, notes: &[&'static str]) {
    let lines = notes
        .iter()
        .map(|note| {
            Line::from(vec![
                Span::styled(" ", THEME.panel_style()),
                Span::styled((*note).to_owned(), THEME.panel_muted_style()),
            ])
            .style(THEME.panel_style())
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_search_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    query: &str,
    query_cursor: usize,
) {
    inference_selector::fill_panel(frame, area);
    let [title_area, input_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(ui::panel_title_line(title))
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        title_area,
    );
    render_filter_input(frame, input_area, query, query_cursor);
}

fn render_filter_input(frame: &mut Frame<'_>, area: Rect, query: &str, query_cursor: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(Block::default().style(search_style()), area);
    let lines = vec![
        search_padding_line(),
        search_input_line(query, query_cursor),
        search_padding_line(),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .style(search_style())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn search_input_line(query: &str, query_cursor: usize) -> Line<'static> {
    let mut spans = vec![Span::styled("   ", search_style())];
    if query.is_empty() {
        spans.push(Span::styled("<all>", search_muted_style()));
        spans.push(search_cursor_span());
    } else {
        let query_cursor = query_cursor.min(char_count(query));
        let byte_index = byte_index_at_char(query, query_cursor);
        let before = &query[..byte_index];
        let after = &query[byte_index..];
        if !before.is_empty() {
            spans.push(Span::styled(before.to_owned(), search_focus_style()));
        }
        spans.push(search_cursor_span());
        if !after.is_empty() {
            spans.push(Span::styled(after.to_owned(), search_focus_style()));
        }
    }
    spans.push(Span::styled(" ", search_style()));
    Line::from(spans).style(search_style())
}

fn search_padding_line() -> Line<'static> {
    Line::from(Span::styled(" ", search_style())).style(search_style())
}

fn search_style() -> Style {
    THEME.surface_style()
}

fn search_muted_style() -> Style {
    Style::default().fg(THEME.muted_text()).bg(THEME.surface())
}

fn search_focus_style() -> Style {
    THEME.focus_style().bg(THEME.surface())
}

fn search_cursor_span() -> Span<'static> {
    Span::styled("█", Style::default().fg(THEME.accent()).bg(THEME.surface()))
}

#[derive(Clone, Debug)]
struct ProviderModelEntry {
    provider_key: String,
    model_id: String,
    model_name: String,
    model: ModelView,
}

fn config(snapshot: Option<&ClientSnapshot>) -> Option<&ConfigView> {
    snapshot.and_then(|snapshot| snapshot.config.current.as_ref())
}

fn filtered_model_entries(
    snapshot: Option<&ClientSnapshot>,
    query: &str,
) -> Vec<ProviderModelEntry> {
    let needle = query.trim().to_lowercase();
    let mut entries = Vec::new();
    if let Some(config) = config(snapshot) {
        for (provider_key, provider) in &config.providers {
            for (model_id, model) in &provider.models {
                if needle.is_empty()
                    || provider_key.to_lowercase().contains(&needle)
                    || provider.name.to_lowercase().contains(&needle)
                    || model_id.to_lowercase().contains(&needle)
                    || model.name.to_lowercase().contains(&needle)
                {
                    entries.push(ProviderModelEntry {
                        provider_key: provider_key.clone(),
                        model_id: model_id.clone(),
                        model_name: model.name.clone(),
                        model: model.clone(),
                    });
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.provider_key
            .cmp(&right.provider_key)
            .then_with(|| left.model_id.cmp(&right.model_id))
    });
    entries
}

fn reasoning_efforts_for_model(model: &ModelView) -> Vec<String> {
    let mut efforts = vec![String::new()];
    efforts.extend(model.options.reasoning_efforts.iter().cloned());
    efforts
}
