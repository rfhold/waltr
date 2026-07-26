//! Runtime configuration editor for session detail routes.

use std::collections::BTreeSet;

use waltr_client::{
    ClientCommand, ClientSnapshot, ConfigView, ModelView, RuntimeConfigMatchView, SessionView,
};
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
        runtime_selector::{self, McpSelectorEntry, ModelSelectorEntry},
    },
    theme::THEME,
    ui,
};

use super::SessionsSection;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RuntimeEditor {
    Provider {
        query: String,
        query_cursor: usize,
        selected_model: usize,
    },
    Mcp {
        query: String,
        query_cursor: usize,
        selected: usize,
        enabled: BTreeSet<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuntimeEditorKind {
    Provider,
    Mcp,
}

impl SessionsSection {
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

    /// Open provider/model selection for the active detail session.
    pub fn open_provider_runtime_editor(
        &mut self,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        if self.detail_session_id.is_none() {
            return SectionAction::Continue;
        }

        self.open_runtime_editor(RuntimeEditorKind::Provider, snapshot)
    }

    /// Open MCP server selection for the active detail session.
    pub fn open_mcp_runtime_editor(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        if self.detail_session_id.is_none() {
            return SectionAction::Continue;
        }

        self.open_runtime_editor(RuntimeEditorKind::Mcp, snapshot)
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

    pub(super) fn handle_runtime_editor_key(
        &mut self,
        key: KeyEvent,
        snapshot: Option<&ClientSnapshot>,
    ) -> SectionAction {
        let detail_session = self.current_detail_session(snapshot);
        let Some(editor) = &mut self.runtime_editor else {
            return SectionAction::Unhandled;
        };

        let action = match editor {
            RuntimeEditor::Provider {
                query,
                query_cursor,
                selected_model,
            } => handle_provider_runtime_editor_key(
                snapshot,
                key,
                query,
                query_cursor,
                selected_model,
            ),
            RuntimeEditor::Mcp {
                query,
                query_cursor,
                selected,
                enabled,
            } => handle_mcp_runtime_editor_key(
                snapshot,
                detail_session.as_ref(),
                key,
                query,
                query_cursor,
                selected,
                enabled,
            ),
        };

        match action {
            RuntimeEditorKeyAction::Continue => SectionAction::Continue,
            RuntimeEditorKeyAction::Close => {
                self.runtime_editor = None;
                SectionAction::Continue
            }
            RuntimeEditorKeyAction::SaveProvider => self.save_provider_runtime(snapshot),
            RuntimeEditorKeyAction::SaveMcp => self.save_mcp_runtime(snapshot),
        }
    }

    pub(super) fn open_provider_editor(&mut self, snapshot: Option<&ClientSnapshot>) {
        if filtered_model_entries(snapshot, "").is_empty() {
            return;
        }
        self.runtime_update.clear_error();
        self.runtime_editor = Some(RuntimeEditor::Provider {
            query: String::new(),
            query_cursor: 0,
            selected_model: 0,
        });
    }

    pub(super) fn cycle_runtime_reasoning(
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
            .runtime_config
            .as_ref()
            .and_then(|runtime| runtime.provider.as_ref())
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
            provider_key: provider.key.clone(),
            model_id: provider.model_id.clone(),
            reasoning_effort,
            enabled_mcp_server_keys,
        })
    }

    pub(super) fn open_mcp_editor(&mut self, snapshot: Option<&ClientSnapshot>) {
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
            query_cursor: 0,
            selected: 0,
            enabled,
        });
    }

    fn save_provider_runtime(&mut self, snapshot: Option<&ClientSnapshot>) -> SectionAction {
        let Some(session) = self.current_detail_session(snapshot) else {
            return SectionAction::Continue;
        };
        let Some(RuntimeEditor::Provider {
            query,
            selected_model,
            ..
        }) = &self.runtime_editor
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
            .runtime_config
            .as_ref()
            .and_then(|runtime| runtime.provider.as_ref())
            .and_then(|provider| provider.reasoning_effort.clone())
            .filter(|effort| entry.model.options.reasoning_efforts.contains(effort));
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
            provider_key: entry.provider_key,
            model_id: entry.model_id,
            reasoning_effort,
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
            reasoning_effort: provider.reasoning_effort.clone(),
            enabled_mcp_server_keys: enabled.iter().cloned().collect(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeEditorKeyAction {
    Continue,
    Close,
    SaveProvider,
    SaveMcp,
}

fn handle_provider_runtime_editor_key(
    snapshot: Option<&ClientSnapshot>,
    key: KeyEvent,
    query: &mut String,
    query_cursor: &mut usize,
    selected_model: &mut usize,
) -> RuntimeEditorKeyAction {
    match key.code {
        KeyCode::Esc => RuntimeEditorKeyAction::Close,
        KeyCode::Up => {
            *selected_model = selected_model.saturating_sub(1);
            RuntimeEditorKeyAction::Continue
        }
        KeyCode::Down => {
            let max = filtered_model_entries(snapshot, query)
                .len()
                .saturating_sub(1);
            *selected_model = (*selected_model + 1).min(max);
            RuntimeEditorKeyAction::Continue
        }
        KeyCode::Enter => RuntimeEditorKeyAction::SaveProvider,
        _ => {
            if handle_search_key(query, query_cursor, key) == SearchKeyAction::TextChanged {
                *selected_model = 0;
            }
            RuntimeEditorKeyAction::Continue
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "key routing mutates independent MCP editor fields"
)]
fn handle_mcp_runtime_editor_key(
    snapshot: Option<&ClientSnapshot>,
    detail_session: Option<&SessionView>,
    key: KeyEvent,
    query: &mut String,
    query_cursor: &mut usize,
    selected: &mut usize,
    enabled: &mut BTreeSet<String>,
) -> RuntimeEditorKeyAction {
    match key.code {
        KeyCode::Esc => RuntimeEditorKeyAction::Close,
        KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            RuntimeEditorKeyAction::Continue
        }
        KeyCode::Down => {
            let max = filtered_mcp_entries(snapshot, detail_session, query)
                .len()
                .saturating_sub(1);
            *selected = (*selected + 1).min(max);
            RuntimeEditorKeyAction::Continue
        }
        KeyCode::Char(' ') => {
            if let Some(item) = filtered_mcp_entries(snapshot, detail_session, query).get(*selected)
                && item.configured
            {
                if enabled.contains(&item.key) {
                    enabled.remove(&item.key);
                } else {
                    enabled.insert(item.key.clone());
                }
            }
            RuntimeEditorKeyAction::Continue
        }
        KeyCode::Enter => RuntimeEditorKeyAction::SaveMcp,
        _ => {
            if handle_search_key(query, query_cursor, key) == SearchKeyAction::TextChanged {
                *selected = 0;
            }
            RuntimeEditorKeyAction::Continue
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

pub(super) fn render_runtime_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) {
    let modal_area = ui::centered_percent_rect(area, 82, 42);
    frame.render_widget(Clear, area);
    frame.render_widget(Clear, modal_area);

    let title = match section.runtime_editor {
        Some(RuntimeEditor::Provider { .. }) => " Provider/model ",
        Some(RuntimeEditor::Mcp { .. }) => " MCP servers ",
        None => return,
    };
    let footer = match section.runtime_editor {
        Some(RuntimeEditor::Provider { .. }) => " Esc cancel  type search  Enter save ",
        Some(RuntimeEditor::Mcp { .. }) => " Esc cancel  type search  Space toggle  Enter save ",
        None => return,
    };
    let editor_area = ui::render_modal_frame(frame, modal_area, title, footer);

    let session = section.current_detail_session(snapshot);
    match &section.runtime_editor {
        Some(RuntimeEditor::Provider {
            query,
            query_cursor,
            selected_model,
        }) => render_provider_editor(
            frame,
            editor_area,
            snapshot,
            session.as_ref(),
            query,
            *query_cursor,
            *selected_model,
        ),
        Some(RuntimeEditor::Mcp {
            query,
            query_cursor,
            selected,
            enabled,
        }) => render_mcp_editor(
            frame,
            editor_area,
            snapshot,
            session.as_ref(),
            query,
            *query_cursor,
            *selected,
            enabled,
        ),
        None => {}
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

    runtime_selector::fill_panel(frame, area);
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
    runtime_selector::render_model_selector(frame, body_area, "Models", &selector_entries);
}

#[allow(
    clippy::too_many_arguments,
    reason = "render helper receives independent MCP editor inputs"
)]
fn render_mcp_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&ClientSnapshot>,
    session: Option<&SessionView>,
    query: &str,
    query_cursor: usize,
    selected: usize,
    enabled: &BTreeSet<String>,
) {
    let entries = filtered_mcp_entries(snapshot, session, query);

    runtime_selector::fill_panel(frame, area);
    let [note_area, note_gap, search_area, search_gap, servers_area] = Layout::vertical([
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
        &["Missing servers stay visible so drift is explicit before you save."],
    );
    ui::render_panel_gap(frame, note_gap);
    render_search_panel(frame, search_area, "Search", query, query_cursor);
    ui::render_panel_gap(frame, search_gap);
    let selector_entries = mcp_selector_entries(&entries, selected, enabled);
    runtime_selector::render_mcp_selector(frame, servers_area, "MCP Servers", &selector_entries);
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
                .and_then(|session| session.runtime_config.as_ref())
                .and_then(|runtime| runtime.provider.as_ref())
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

fn mcp_selector_entries(
    entries: &[McpEntry],
    selected: usize,
    enabled: &BTreeSet<String>,
) -> Vec<McpSelectorEntry> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| McpSelectorEntry {
            key: entry.key.clone(),
            focused: index == selected,
            checked: enabled.contains(&entry.key),
            configured: entry.configured,
            state: entry.state,
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
    runtime_selector::fill_panel(frame, area);
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct McpEntry {
    key: String,
    configured: bool,
    state: RuntimeConfigMatchView,
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

fn reasoning_efforts_for_model(model: &ModelView) -> Vec<String> {
    let mut efforts = vec![String::new()];
    efforts.extend(model.options.reasoning_efforts.iter().cloned());
    efforts
}
