//! Shared runtime provider/model and MCP selector rendering.

use waltr_client::RuntimeConfigMatchView;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

use crate::{
    theme::{StatusRole, THEME},
    ui,
};

/// Provider/model row rendered by runtime selectors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ModelSelectorEntry {
    /// Provider key for the model.
    pub provider_key: String,
    /// Provider model id.
    pub model_id: String,
    /// Human-readable model name, when configured.
    pub model_name: String,
    /// Whether keyboard focus is on this row.
    pub focused: bool,
    /// Optional badge for the persisted/current runtime selection.
    pub current_badge: Option<&'static str>,
}

impl ModelSelectorEntry {
    fn display_key(&self) -> String {
        format!("{}/{}", self.provider_key, self.model_id)
    }
}

/// MCP row rendered by runtime selectors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct McpSelectorEntry {
    /// MCP server key.
    pub key: String,
    /// Whether keyboard focus is on this row.
    pub focused: bool,
    /// Whether the server is enabled in the draft selection.
    pub checked: bool,
    /// Whether the server is still present in current config.
    pub configured: bool,
    /// Match state against the runtime config source.
    pub state: RuntimeConfigMatchView,
}

/// Render a provider/model selector panel.
pub(super) fn render_model_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    entries: &[ModelSelectorEntry],
) {
    render_list_panel(frame, area, title, model_choice_lines(entries));
}

/// Render an MCP selector panel.
pub(super) fn render_mcp_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    entries: &[McpSelectorEntry],
) {
    render_list_panel(frame, area, title, mcp_option_lines(entries));
}

/// Fill an area with the standard raised panel color.
pub(super) fn fill_panel(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Block::default().style(THEME.panel_style()), area);
}

fn render_list_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    lines: Vec<Line<'static>>,
) {
    fill_panel(frame, area);
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut panel_lines = vec![ui::panel_title_line(title)];
    panel_lines.extend(lines);
    frame.render_widget(
        Paragraph::new(panel_lines)
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn model_choice_lines(entries: &[ModelSelectorEntry]) -> Vec<Line<'static>> {
    if entries.is_empty() {
        return vec![muted_panel_line("No models match this search.")];
    }

    entries
        .iter()
        .map(|entry| {
            let focused = entry.focused;
            let mut spans = vec![
                option_marker(focused),
                Span::raw(" "),
                Span::styled(entry.display_key(), option_title_style(focused)),
            ];
            if !entry.model_name.is_empty() && entry.model_name != entry.model_id {
                spans.extend([
                    Span::raw("  "),
                    Span::styled(entry.model_name.clone(), option_muted_style(focused)),
                ]);
            }
            if focused {
                spans.extend([Span::raw("  "), selected_badge(focused)]);
            }
            if let Some(label) = entry.current_badge {
                spans.extend([
                    Span::raw("  "),
                    badge(label, THEME.status_style(StatusRole::Ready), focused),
                ]);
            }
            option_line(spans, focused)
        })
        .collect()
}

fn mcp_option_lines(entries: &[McpSelectorEntry]) -> Vec<Line<'static>> {
    if entries.is_empty() {
        return vec![muted_panel_line("No MCP servers match this search.")];
    }

    entries
        .iter()
        .map(|entry| {
            let focused = entry.focused;
            let checkbox = if entry.checked { "[x]" } else { "[ ]" };
            let availability = match (entry.configured, entry.checked) {
                (false, _) => "missing",
                (true, true) => "enabled",
                (true, false) => "available",
            };
            let title_style = if entry.configured {
                option_title_style(focused)
            } else {
                option_danger_style(focused)
            };
            let mut spans = vec![
                option_marker(focused),
                Span::raw(" "),
                Span::styled(checkbox, option_title_style(focused)),
                Span::raw(" "),
                Span::styled(entry.key.clone(), title_style),
                Span::raw("  "),
                status_badge(availability, entry.configured, focused),
                Span::raw(" "),
                runtime_match_badge(entry.state, focused),
            ];
            if focused {
                spans.extend([Span::raw("  "), selected_badge(focused)]);
            }
            option_line(spans, focused)
        })
        .collect()
}

fn panel_line(mut spans: Vec<Span<'static>>) -> Line<'static> {
    let mut prefixed = vec![Span::styled(" ", THEME.panel_style())];
    prefixed.append(&mut spans);
    Line::from(prefixed).style(THEME.panel_style())
}

fn muted_panel_line(text: &'static str) -> Line<'static> {
    panel_line(vec![Span::styled(text, THEME.panel_muted_style())])
}

fn option_line(spans: Vec<Span<'static>>, focused: bool) -> Line<'static> {
    panel_line(spans).style(option_style(focused))
}

fn option_marker(focused: bool) -> Span<'static> {
    Span::styled(
        if focused { ">" } else { " " },
        if focused {
            option_focus_style(true).add_modifier(Modifier::BOLD)
        } else {
            option_muted_style(false)
        },
    )
}

fn selected_badge(focused: bool) -> Span<'static> {
    badge("selected", THEME.focus_style(), focused)
}

fn status_badge(label: &'static str, configured: bool, focused: bool) -> Span<'static> {
    let style = if configured {
        THEME.status_style(StatusRole::Ready)
    } else {
        THEME.danger_style()
    };
    badge(label, style, focused)
}

fn runtime_match_badge(state: RuntimeConfigMatchView, focused: bool) -> Span<'static> {
    let (label, role) = runtime_match_label_role(state);
    badge(
        label,
        THEME.status_style(role).add_modifier(Modifier::BOLD),
        focused,
    )
}

fn badge(label: &'static str, style: Style, focused: bool) -> Span<'static> {
    Span::styled(format!("[{label}]"), style.bg(option_bg(focused)))
}

fn option_style(focused: bool) -> Style {
    THEME.panel_style().bg(option_bg(focused))
}

fn option_title_style(focused: bool) -> Style {
    option_style(focused).add_modifier(Modifier::BOLD)
}

fn option_muted_style(focused: bool) -> Style {
    THEME.panel_muted_style().bg(option_bg(focused))
}

fn option_focus_style(focused: bool) -> Style {
    THEME.focus_style().bg(option_bg(focused))
}

fn option_danger_style(focused: bool) -> Style {
    THEME.danger_style().bg(option_bg(focused))
}

fn option_bg(focused: bool) -> ratatui::style::Color {
    if focused {
        THEME.dim_accent()
    } else {
        THEME.panel()
    }
}

const fn runtime_match_label_role(state: RuntimeConfigMatchView) -> (&'static str, StatusRole) {
    match state {
        RuntimeConfigMatchView::Configured => ("configured", StatusRole::Ready),
        RuntimeConfigMatchView::Missing => ("missing", StatusRole::Error),
        RuntimeConfigMatchView::Changed => ("changed", StatusRole::Saving),
        RuntimeConfigMatchView::Unspecified | RuntimeConfigMatchView::Unknown => {
            ("unknown", StatusRole::Loading)
        }
    }
}
