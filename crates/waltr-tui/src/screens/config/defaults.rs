//! Runtime defaults config section.

use std::collections::BTreeSet;

use waltr_client::{
    ClientCommand, ClientOperationView, ClientSnapshot, ConfigView, McpServerTypeView,
    McpServerView, RuntimeDefaultsView,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use crate::{
    screens::{OperationResult, OperationTracker},
    theme::THEME,
    ui,
};

use super::SectionAction;

/// Runtime defaults section state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDefaultsSection {
    selected_provider: usize,
    selected_model: usize,
    selected_mcp: usize,
    draft: Option<RuntimeDefaultsDraft>,
    patch: OperationTracker,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeDefaultsDraft {
    provider_key: String,
    model_id: String,
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
        false
    }

    /// Dismiss active overlay, if any.
    pub const fn dismiss_overlay(&mut self) -> bool {
        false
    }

    /// No-op create hook used by the config shell leader command.
    pub const fn open_create_form(&mut self) {}

    /// Route a key event into the section.
    pub fn handle_key(&mut self, key: KeyEvent, config: Option<&ConfigView>) -> SectionAction {
        if self.patch.is_pending() {
            return SectionAction::Continue;
        }

        match key.code {
            KeyCode::Up => {
                self.selected_provider = self.selected_provider.saturating_sub(1);
                self.selected_model = 0;
                SectionAction::Continue
            }
            KeyCode::Down => {
                let max = provider_count(config).saturating_sub(1);
                self.selected_provider = (self.selected_provider + 1).min(max);
                self.selected_model = 0;
                SectionAction::Continue
            }
            KeyCode::Left => {
                self.selected_model = self.selected_model.saturating_sub(1);
                SectionAction::Continue
            }
            KeyCode::Right => {
                let max = model_count(config, self.selected_provider).saturating_sub(1);
                self.selected_model = (self.selected_model + 1).min(max);
                SectionAction::Continue
            }
            KeyCode::Char(' ') => {
                self.toggle_all_mcp(config);
                SectionAction::Continue
            }
            KeyCode::Char('m') => {
                self.toggle_selected_mcp(config);
                SectionAction::Continue
            }
            KeyCode::Char('j') => {
                let max = mcp_count(config).saturating_sub(1);
                self.selected_mcp = (self.selected_mcp + 1).min(max);
                SectionAction::Continue
            }
            KeyCode::Char('k') => {
                self.selected_mcp = self.selected_mcp.saturating_sub(1);
                SectionAction::Continue
            }
            KeyCode::Enter => self.save(config),
            KeyCode::Backspace | KeyCode::Delete => self.clear(),
            _ => SectionAction::Unhandled,
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

    fn save(&mut self, config: Option<&ConfigView>) -> SectionAction {
        let provider_key = selected_provider_key(config, self.selected_provider)
            .map_or_else(String::new, ToOwned::to_owned);
        let model_id = selected_model_id(config, self.selected_provider, self.selected_model)
            .map_or_else(String::new, ToOwned::to_owned);
        let mut draft = self.draft(config);
        draft.provider_key = provider_key;
        draft.model_id = model_id;

        self.patch.begin();
        SectionAction::ClientCommand(ClientCommand::SetRuntimeDefaults {
            defaults: Some(RuntimeDefaultsView {
                provider_key: draft.provider_key,
                model_id: draft.model_id,
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
        let lines = if loaded {
            config.map_or_else(loading_lines, |config| self.config_lines(config))
        } else {
            loading_lines()
        };

        frame.render_widget(
            Paragraph::new(lines)
                .style(THEME.text_style())
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    /// Defaults section has no overlay.
    pub const fn render_overlay(&self, _frame: &mut Frame<'_>, _area: Rect) {}

    fn config_lines(&self, config: &ConfigView) -> Vec<Line<'static>> {
        let draft = self.draft(Some(config));
        let provider =
            selected_provider_key(Some(config), self.selected_provider).unwrap_or("none");
        let model = selected_model_id(Some(config), self.selected_provider, self.selected_model)
            .unwrap_or("none");
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

        let mut lines = vec![
            Line::from(vec![
                ui::selected_title("Provider"),
                Span::raw(" "),
                Span::styled(provider.to_owned(), THEME.focus_style()),
            ]),
            Line::from(vec![
                ui::selected_title("Model"),
                Span::raw(" "),
                Span::styled(model.to_owned(), THEME.focus_style()),
            ]),
            Line::from(vec![
                ui::selected_title("MCP"),
                Span::raw(" "),
                Span::styled(mcp_mode, THEME.focus_style()),
            ]),
            Line::from(""),
        ];

        if config.mcp_servers.is_empty() {
            lines.push(ui::loading_line("No MCP servers configured."));
        } else {
            lines.push(Line::from("MCP servers"));
            for (index, (key, server)) in config.mcp_servers.iter().enumerate() {
                lines.push(mcp_server_line(
                    key,
                    server,
                    index == self.selected_mcp,
                    draft.mcp_default_enabled,
                    draft.enabled_mcp_server_keys.contains(key),
                ));
            }
        }

        lines.push(Line::from(""));
        lines.push(ui::footer_hint_line(
            "Up/Down provider  Left/Right model  Space all MCP  j/k MCP  m toggle MCP  Enter save  Del clear",
        ));

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
}

impl Default for RuntimeDefaultsSection {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_index(index: usize, count: usize) -> usize {
    if count == 0 { 0 } else { index.min(count - 1) }
}

fn provider_count(config: Option<&ConfigView>) -> usize {
    config.map_or(0, |config| config.providers.len())
}

fn model_count(config: Option<&ConfigView>, provider_index: usize) -> usize {
    selected_provider_key(config, provider_index)
        .and_then(|key| config?.providers.get(key))
        .map_or(0, |provider| provider.models.len())
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

fn selected_mcp_key(config: Option<&ConfigView>, index: usize) -> Option<&str> {
    config?.mcp_servers.keys().nth(index).map(String::as_str)
}

fn mcp_server_line(
    key: &str,
    server: &McpServerView,
    selected: bool,
    all_enabled: bool,
    enabled: bool,
) -> Line<'static> {
    let display = if server.name.is_empty() {
        key.to_owned()
    } else {
        server.name.clone()
    };
    let check = if all_enabled || enabled { "[x]" } else { "[ ]" };

    Line::from(vec![
        ui::selection_marker(selected),
        Span::raw(" "),
        Span::styled(check, THEME.accent_style()),
        Span::raw(" "),
        ui::selected_title(display),
        Span::raw("  "),
        Span::styled(mcp_type_name(server.kind), THEME.focus_style()),
        Span::raw("  "),
        Span::styled(mcp_target(server), THEME.text_style()),
    ])
}

const fn mcp_type_name(kind: McpServerTypeView) -> &'static str {
    match kind {
        McpServerTypeView::Stdio => "stdio",
        McpServerTypeView::Sse => "sse",
        McpServerTypeView::Unspecified => "unspecified",
        McpServerTypeView::Unknown => "unknown",
    }
}

fn mcp_target(server: &McpServerView) -> String {
    match server.kind {
        McpServerTypeView::Sse => {
            if server.url.is_empty() {
                "no-url".to_owned()
            } else {
                server.url.clone()
            }
        }
        _ => {
            if server.command.is_empty() {
                "no-command".to_owned()
            } else {
                server.command.clone()
            }
        }
    }
}

fn loading_lines() -> Vec<Line<'static>> {
    vec![ui::loading_line("Waiting for runtime defaults...")]
}
