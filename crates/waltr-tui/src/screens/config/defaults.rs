//! Runtime defaults config section.

use std::collections::BTreeSet;

use waltr_client::{
    ClientCommand, ClientOperationView, ClientSnapshot, ConfigView, RuntimeConfigMatchView,
    RuntimeDefaultsView,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::{
    screens::{
        OperationResult, OperationTracker,
        runtime_selector::{self, McpSelectorEntry, ModelSelectorEntry},
    },
    theme::THEME,
    ui,
};

use crate::screens::SectionAction;

/// Runtime defaults section state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDefaultsSection {
    selected_provider: usize,
    selected_model: usize,
    selected_mcp: usize,
    overlay: Option<RuntimeDefaultsOverlay>,
    draft: Option<RuntimeDefaultsDraft>,
    patch: OperationTracker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeDefaultsOverlay {
    Model,
    Mcp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeDefaultsDraft {
    provider_key: String,
    model_id: String,
    reasoning_effort: Option<String>,
    mcp_default_enabled: bool,
    enabled_mcp_server_keys: BTreeSet<String>,
}

impl RuntimeDefaultsSection {
    /// Create an empty runtime defaults section.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected_provider: 0,
            selected_model: 0,
            selected_mcp: 0,
            overlay: None,
            draft: None,
            patch: OperationTracker::new(),
        }
    }

    /// Reconcile pending patch results.
    pub fn reconcile_patch(&mut self, snapshot: &ClientSnapshot) {
        if let Some(result) = self
            .patch
            .reconcile(&snapshot.config.patch, snapshot, |operation| {
                let ClientOperationView::ConfigPatch { status, error, .. } = operation else {
                    return None;
                };

                Some(match *status {
                    "ready" => OperationResult::Ready(()),
                    "failed" => OperationResult::Failed(
                        error.as_deref().unwrap_or("config patch failed").to_owned(),
                    ),
                    _ => OperationResult::Pending,
                })
            })
            && result.is_ok()
        {
            self.draft = None;
        }
    }

    /// Clamp selections to available config entries.
    pub fn sync_selection(&mut self, config: Option<&ConfigView>) {
        if self.draft.is_none()
            && let Some(defaults) = config.and_then(|config| config.runtime_defaults.as_ref())
            && let Some((provider_index, model_index)) = model_indices(
                config,
                defaults.provider_key.as_str(),
                defaults.model_id.as_str(),
            )
        {
            self.selected_provider = provider_index;
            self.selected_model = model_index;
        }

        let provider_count = config.map_or(0, |config| config.providers.len());
        self.selected_provider = clamp_index(self.selected_provider, provider_count);
        let model_count = selected_provider_key(config, self.selected_provider)
            .and_then(|key| config?.providers.get(key))
            .map_or(0, |provider| provider.models.len());
        self.selected_model = clamp_index(self.selected_model, model_count);
        let mcp_count = config.map_or(0, |config| config.mcp_servers.len());
        self.selected_mcp = clamp_index(self.selected_mcp, mcp_count);
    }

    /// Whether the section has an active overlay.
    #[must_use]
    pub const fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    /// Dismiss active overlay, if any.
    pub fn dismiss_overlay(&mut self) -> bool {
        if self.patch.is_pending() {
            return false;
        }

        self.overlay.take().is_some()
    }

    /// Open the default model selector from the config shell leader command.
    pub fn open_create_form(&mut self) {
        if !self.patch.is_pending() {
            self.overlay = Some(RuntimeDefaultsOverlay::Model);
        }
    }

    /// Route a key event into the section.
    pub fn handle_key(&mut self, key: KeyEvent, config: Option<&ConfigView>) -> SectionAction {
        if self.patch.is_pending() {
            return SectionAction::Continue;
        }

        if let Some(overlay) = self.overlay {
            return self.handle_overlay_key(overlay, key, config);
        }

        match key.code {
            KeyCode::Char('m') => {
                self.overlay = Some(RuntimeDefaultsOverlay::Model);
                SectionAction::Continue
            }
            KeyCode::Char(' ') => {
                self.overlay = Some(RuntimeDefaultsOverlay::Mcp);
                SectionAction::Continue
            }
            KeyCode::Char('r') => {
                self.cycle_reasoning_effort(config);
                SectionAction::Continue
            }
            KeyCode::Enter => self.save(config),
            KeyCode::Backspace | KeyCode::Delete => self.clear(),
            _ => SectionAction::Unhandled,
        }
    }

    fn handle_overlay_key(
        &mut self,
        overlay: RuntimeDefaultsOverlay,
        key: KeyEvent,
        config: Option<&ConfigView>,
    ) -> SectionAction {
        match overlay {
            RuntimeDefaultsOverlay::Model => match key.code {
                KeyCode::Up | KeyCode::Left | KeyCode::Char('k') => {
                    self.move_selected_model(config, -1);
                    SectionAction::Continue
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Char('j') => {
                    self.move_selected_model(config, 1);
                    SectionAction::Continue
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    SectionAction::Continue
                }
                _ => SectionAction::Unhandled,
            },
            RuntimeDefaultsOverlay::Mcp => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected_mcp = self.selected_mcp.saturating_sub(1);
                    SectionAction::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = mcp_count(config).saturating_sub(1);
                    self.selected_mcp = (self.selected_mcp + 1).min(max);
                    SectionAction::Continue
                }
                KeyCode::Char(' ') | KeyCode::Char('m') => {
                    self.toggle_selected_mcp(config);
                    SectionAction::Continue
                }
                KeyCode::Char('a') => {
                    self.toggle_all_mcp(config);
                    SectionAction::Continue
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    SectionAction::Continue
                }
                _ => SectionAction::Unhandled,
            },
        }
    }

    fn move_selected_model(&mut self, config: Option<&ConfigView>, delta: isize) {
        let entries = model_entries(config);
        if entries.is_empty() {
            self.selected_provider = 0;
            self.selected_model = 0;
            return;
        }

        let current = selected_model_entry_index(
            config,
            self.selected_provider,
            self.selected_model,
            &entries,
        );
        let next = current.saturating_add_signed(delta).min(entries.len() - 1);
        if let Some((provider_index, model_index)) = model_indices(
            config,
            entries[next].provider_key.as_str(),
            entries[next].model_id.as_str(),
        ) {
            self.selected_provider = provider_index;
            self.selected_model = model_index;
        }
    }

    fn draft(&self, config: Option<&ConfigView>) -> RuntimeDefaultsDraft {
        self.draft.clone().unwrap_or_else(|| RuntimeDefaultsDraft {
            provider_key: config
                .and_then(|config| config.runtime_defaults.as_ref())
                .map_or_else(String::new, |defaults| defaults.provider_key.clone()),
            model_id: config
                .and_then(|config| config.runtime_defaults.as_ref())
                .map_or_else(String::new, |defaults| defaults.model_id.clone()),
            reasoning_effort: config
                .and_then(|config| config.runtime_defaults.as_ref())
                .and_then(|defaults| defaults.reasoning_effort.clone()),
            mcp_default_enabled: config
                .and_then(|config| config.runtime_defaults.as_ref())
                .is_some_and(|defaults| defaults.mcp_default_enabled),
            enabled_mcp_server_keys: config
                .and_then(|config| config.runtime_defaults.as_ref())
                .map_or_else(BTreeSet::new, |defaults| {
                    defaults.enabled_mcp_server_keys.iter().cloned().collect()
                }),
        })
    }

    fn toggle_all_mcp(&mut self, config: Option<&ConfigView>) {
        let mut draft = self.draft(config);
        draft.mcp_default_enabled = !draft.mcp_default_enabled;
        if draft.mcp_default_enabled {
            draft.enabled_mcp_server_keys.clear();
        }
        self.draft = Some(draft);
    }

    fn toggle_selected_mcp(&mut self, config: Option<&ConfigView>) {
        let Some(key) = selected_mcp_key(config, self.selected_mcp) else {
            return;
        };
        let mut draft = self.draft(config);
        draft.mcp_default_enabled = false;
        if !draft.enabled_mcp_server_keys.insert(key.to_owned()) {
            draft.enabled_mcp_server_keys.remove(key);
        }
        self.draft = Some(draft);
    }

    fn cycle_reasoning_effort(&mut self, config: Option<&ConfigView>) {
        let efforts =
            selected_reasoning_efforts(config, self.selected_provider, self.selected_model);
        if efforts.is_empty() {
            return;
        }

        let mut draft = self.draft(config);
        draft.reasoning_effort = match draft
            .reasoning_effort
            .as_ref()
            .and_then(|effort| efforts.iter().position(|value| value == effort))
        {
            Some(index) if index + 1 < efforts.len() => Some(efforts[index + 1].clone()),
            Some(_) => None,
            None => efforts.first().cloned(),
        };
        self.draft = Some(draft);
    }

    fn save(&mut self, config: Option<&ConfigView>) -> SectionAction {
        let provider_key = selected_provider_key(config, self.selected_provider)
            .map_or_else(String::new, ToOwned::to_owned);
        let model_id = selected_model_id(config, self.selected_provider, self.selected_model)
            .map_or_else(String::new, ToOwned::to_owned);
        let efforts =
            selected_reasoning_efforts(config, self.selected_provider, self.selected_model);
        let mut draft = self.draft(config);
        draft.provider_key = provider_key;
        draft.model_id = model_id;
        if draft
            .reasoning_effort
            .as_ref()
            .is_some_and(|effort| !efforts.contains(effort))
        {
            draft.reasoning_effort = None;
        }

        self.patch.begin();
        SectionAction::ClientCommand(ClientCommand::SetRuntimeDefaults {
            defaults: Some(RuntimeDefaultsView {
                provider_key: draft.provider_key,
                model_id: draft.model_id,
                reasoning_effort: draft.reasoning_effort,
                mcp_default_enabled: draft.mcp_default_enabled,
                enabled_mcp_server_keys: draft.enabled_mcp_server_keys.into_iter().collect(),
            }),
        })
    }

    fn clear(&mut self) -> SectionAction {
        self.patch.begin();
        SectionAction::ClientCommand(ClientCommand::SetRuntimeDefaults {
            defaults: Some(RuntimeDefaultsView {
                provider_key: String::new(),
                model_id: String::new(),
                reasoning_effort: None,
                mcp_default_enabled: false,
                enabled_mcp_server_keys: Vec::new(),
            }),
        })
    }

    /// Render section content.
    pub fn render_content(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        config: Option<&ConfigView>,
        loaded: bool,
    ) {
        if !loaded {
            frame.render_widget(loading_widget(), area);
            return;
        }

        let Some(config) = config else {
            frame.render_widget(loading_widget(), area);
            return;
        };

        self.render_loaded_content(frame, area, config);
    }

    fn render_loaded_content(&self, frame: &mut Frame<'_>, area: Rect, config: &ConfigView) {
        runtime_selector::fill_panel(frame, area);

        let status_lines = self.status_lines(config);
        let status_height = status_lines.len().try_into().unwrap_or(u16::MAX);
        let [summary_area, status_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(status_height)]).areas(area);

        frame.render_widget(
            Paragraph::new(self.summary_lines(config))
                .style(THEME.panel_style())
                .wrap(Wrap { trim: false }),
            summary_area,
        );

        if !status_lines.is_empty() {
            frame.render_widget(
                Paragraph::new(status_lines)
                    .style(THEME.panel_style())
                    .wrap(Wrap { trim: false }),
                status_area,
            );
        }
    }

    /// Render the active defaults selector overlay, if any.
    pub fn render_overlay(&self, frame: &mut Frame<'_>, area: Rect, config: Option<&ConfigView>) {
        let Some(overlay) = self.overlay else {
            return;
        };

        let (title, footer, height) = match overlay {
            RuntimeDefaultsOverlay::Model => (
                "Select default model",
                "Up/Down choose  Enter done  Esc cancel",
                16,
            ),
            RuntimeDefaultsOverlay::Mcp => (
                "Select default MCP",
                "Up/Down choose  Space toggle  a all  Enter done  Esc cancel",
                16,
            ),
        };
        let selector_area = ui::centered_percent_rect(area, 82, height);
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_widget(ratatui::widgets::Clear, selector_area);
        let body_area = super::render_modal_frame(frame, selector_area, title, footer);

        match (overlay, config) {
            (RuntimeDefaultsOverlay::Model, Some(config)) => {
                let models = self.model_selector_entries(config);
                runtime_selector::render_model_selector(frame, body_area, "Models", &models);
            }
            (RuntimeDefaultsOverlay::Mcp, Some(config)) => {
                let mcp_servers = self.mcp_selector_entries(config);
                runtime_selector::render_mcp_selector(
                    frame,
                    body_area,
                    "MCP Servers",
                    &mcp_servers,
                );
            }
            (RuntimeDefaultsOverlay::Model, None) => {
                runtime_selector::render_model_selector(frame, body_area, "Models", &[]);
            }
            (RuntimeDefaultsOverlay::Mcp, None) => {
                runtime_selector::render_mcp_selector(frame, body_area, "MCP Servers", &[]);
            }
        }
    }

    fn summary_lines(&self, config: &ConfigView) -> Vec<Line<'static>> {
        let draft = self.draft(Some(config));
        let selected_model =
            selected_model_entry(Some(config), self.selected_provider, self.selected_model)
                .map_or_else(|| "none".to_owned(), RuntimeModelEntry::display_key);
        let mcp_mode = if draft.mcp_default_enabled {
            "all MCP servers".to_owned()
        } else if draft.enabled_mcp_server_keys.is_empty() {
            "no MCP servers".to_owned()
        } else {
            draft
                .enabled_mcp_server_keys
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        let reasoning = if selected_reasoning_efforts(
            Some(config),
            self.selected_provider,
            self.selected_model,
        )
        .is_empty()
        {
            "not supported".to_owned()
        } else {
            draft
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "none".to_owned())
        };

        vec![
            panel_line(vec![
                Span::styled("Default model ", THEME.panel_muted_style()),
                Span::styled(selected_model, THEME.focus_style().bg(THEME.panel())),
            ]),
            panel_line(vec![
                Span::styled("Reasoning ", THEME.panel_muted_style()),
                Span::styled(reasoning, THEME.focus_style().bg(THEME.panel())),
            ]),
            panel_line(vec![
                Span::styled("MCP default ", THEME.panel_muted_style()),
                Span::styled(mcp_mode, THEME.focus_style().bg(THEME.panel())),
            ]),
            panel_line(vec![Span::styled(
                "m selects model, Space selects MCP, Enter saves for new sessions.",
                THEME.panel_muted_style(),
            )]),
        ]
    }

    fn status_lines(&self, config: &ConfigView) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.patch.is_pending() {
            lines.push(ui::loading_line("Saving runtime defaults..."));
        }
        if let Some(error) = self.patch.error() {
            lines.push(Line::from(Span::styled(
                format!("Error: {error}"),
                THEME.danger_style(),
            )));
        }
        if config.providers.is_empty() {
            lines.push(ui::loading_line("No providers configured."));
        }

        lines
    }

    fn model_selector_entries(&self, config: &ConfigView) -> Vec<ModelSelectorEntry> {
        let selected =
            selected_model_entry(Some(config), self.selected_provider, self.selected_model);
        let persisted = config.runtime_defaults.as_ref();

        model_entries(Some(config))
            .into_iter()
            .map(|entry| {
                let focused = selected.as_ref().is_some_and(|selected| {
                    selected.provider_key == entry.provider_key
                        && selected.model_id == entry.model_id
                });
                let is_default = persisted.is_some_and(|defaults| {
                    defaults.provider_key == entry.provider_key
                        && defaults.model_id == entry.model_id
                });
                ModelSelectorEntry {
                    provider_key: entry.provider_key,
                    model_id: entry.model_id,
                    model_name: entry.model_name,
                    focused,
                    current_badge: is_default.then_some("default"),
                }
            })
            .collect()
    }

    fn mcp_selector_entries(&self, config: &ConfigView) -> Vec<McpSelectorEntry> {
        let draft = self.draft(Some(config));
        config
            .mcp_servers
            .keys()
            .enumerate()
            .map(|(index, key)| McpSelectorEntry {
                key: key.clone(),
                focused: index == self.selected_mcp,
                checked: draft.mcp_default_enabled || draft.enabled_mcp_server_keys.contains(key),
                configured: true,
                state: RuntimeConfigMatchView::Configured,
            })
            .collect()
    }
}

impl Default for RuntimeDefaultsSection {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_index(index: usize, count: usize) -> usize {
    if count == 0 { 0 } else { index.min(count - 1) }
}

fn panel_line(mut spans: Vec<Span<'static>>) -> Line<'static> {
    let mut prefixed = vec![Span::styled(" ", THEME.panel_style())];
    prefixed.append(&mut spans);
    Line::from(prefixed).style(THEME.panel_style())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeModelEntry {
    provider_key: String,
    model_id: String,
    model_name: String,
}

impl RuntimeModelEntry {
    fn display_key(self) -> String {
        format!("{}/{}", self.provider_key, self.model_id)
    }
}

fn model_entries(config: Option<&ConfigView>) -> Vec<RuntimeModelEntry> {
    let mut entries = Vec::new();
    if let Some(config) = config {
        for (provider_key, provider) in &config.providers {
            for (model_id, model) in &provider.models {
                entries.push(RuntimeModelEntry {
                    provider_key: provider_key.clone(),
                    model_id: model_id.clone(),
                    model_name: model.name.clone(),
                });
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

fn selected_model_entry(
    config: Option<&ConfigView>,
    provider_index: usize,
    model_index: usize,
) -> Option<RuntimeModelEntry> {
    let config = config?;
    let provider_key = selected_provider_key(Some(config), provider_index)?;
    let provider = config.providers.get(provider_key)?;
    let (model_id, model) = provider.models.iter().nth(model_index)?;
    Some(RuntimeModelEntry {
        provider_key: provider_key.to_owned(),
        model_id: model_id.clone(),
        model_name: model.name.clone(),
    })
}

fn selected_model_entry_index(
    config: Option<&ConfigView>,
    provider_index: usize,
    model_index: usize,
    entries: &[RuntimeModelEntry],
) -> usize {
    selected_model_entry(config, provider_index, model_index)
        .and_then(|selected| {
            entries.iter().position(|entry| {
                entry.provider_key == selected.provider_key && entry.model_id == selected.model_id
            })
        })
        .unwrap_or(0)
}

fn model_indices(
    config: Option<&ConfigView>,
    provider_key: &str,
    model_id: &str,
) -> Option<(usize, usize)> {
    let config = config?;
    let provider_index = config
        .providers
        .keys()
        .position(|key| key == provider_key)?;
    let model_index = config
        .providers
        .get(provider_key)?
        .models
        .keys()
        .position(|key| key == model_id)?;
    Some((provider_index, model_index))
}

fn mcp_count(config: Option<&ConfigView>) -> usize {
    config.map_or(0, |config| config.mcp_servers.len())
}

fn selected_provider_key(config: Option<&ConfigView>, index: usize) -> Option<&str> {
    config?.providers.keys().nth(index).map(String::as_str)
}

fn selected_model_id(
    config: Option<&ConfigView>,
    provider_index: usize,
    model_index: usize,
) -> Option<&str> {
    let provider_key = selected_provider_key(config, provider_index)?;
    config?
        .providers
        .get(provider_key)?
        .models
        .keys()
        .nth(model_index)
        .map(String::as_str)
}

fn selected_reasoning_efforts(
    config: Option<&ConfigView>,
    provider_index: usize,
    model_index: usize,
) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let Some(provider_key) = selected_provider_key(Some(config), provider_index) else {
        return Vec::new();
    };
    let Some(model_id) = selected_model_id(Some(config), provider_index, model_index) else {
        return Vec::new();
    };
    config
        .providers
        .get(provider_key)
        .and_then(|provider| provider.models.get(model_id))
        .map_or_else(Vec::new, |model| model.options.reasoning_efforts.clone())
}

fn selected_mcp_key(config: Option<&ConfigView>, index: usize) -> Option<&str> {
    config?.mcp_servers.keys().nth(index).map(String::as_str)
}

fn loading_widget() -> Paragraph<'static> {
    Paragraph::new(vec![ui::loading_line("Waiting for runtime defaults...")])
        .style(THEME.panel_style())
}
