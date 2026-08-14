//! Agent selection overlay shared by session creation and switching.

use waltr_client::{ClientSnapshot, SelectableAgentView};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{theme::THEME, ui};

use super::{SessionsSection, session_active};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AgentSelector {
    pub(super) purpose: AgentSelectorPurpose,
    pub(super) selected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum AgentSelectorPurpose {
    Create,
    Switch { session_id: String },
}

impl AgentSelector {
    pub(super) fn create(snapshot: Option<&ClientSnapshot>) -> Self {
        let selected = snapshot.map_or(0, |snapshot| {
            selected_agent_index(
                &snapshot.sessions.agents.entries,
                &snapshot.sessions.agents.default_agent_name,
            )
        });
        Self {
            purpose: AgentSelectorPurpose::Create,
            selected,
        }
    }

    pub(super) fn switch(session_id: String, agent_name: &str, snapshot: &ClientSnapshot) -> Self {
        Self {
            purpose: AgentSelectorPurpose::Switch { session_id },
            selected: selected_agent_index(&snapshot.sessions.agents.entries, agent_name),
        }
    }

    pub(super) fn selected_agent<'a>(
        &self,
        snapshot: Option<&'a ClientSnapshot>,
    ) -> Option<&'a SelectableAgentView> {
        snapshot?.sessions.agents.entries.get(self.selected)
    }

    pub(super) const fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(super) fn select_next(&mut self, len: usize) {
        self.selected = (self.selected + 1).min(len.saturating_sub(1));
    }

    pub(super) fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

fn selected_agent_index(entries: &[SelectableAgentView], name: &str) -> usize {
    entries
        .iter()
        .position(|entry| entry.name == name)
        .unwrap_or(0)
}

pub(super) fn render_agent_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) {
    let Some(selector) = &section.agent_selector else {
        return;
    };
    let entries = snapshot.map_or(&[][..], |snapshot| {
        snapshot.sessions.agents.entries.as_slice()
    });
    let height = u16::try_from(entries.len().min(8).saturating_add(12)).unwrap_or(20);
    let modal_area = ui::centered_rect(area, 64, height);
    frame.render_widget(Clear, modal_area);

    let switching = matches!(selector.purpose, AgentSelectorPurpose::Switch { .. });
    let pending = if switching {
        section.agent_update.is_pending()
    } else {
        section.create.is_pending()
    };
    let footer = if pending {
        "Saving..."
    } else if switching {
        "Up/Down select  Enter switch  r retry  Esc cancel"
    } else {
        "Up/Down select  Enter create  r retry  Esc cancel"
    };
    let body = ui::render_modal_frame(
        frame,
        modal_area,
        if switching {
            "Switch agent"
        } else {
            "Create session"
        },
        footer,
    );
    let [message_area, list_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(body);

    let message = if switching {
        let active = snapshot
            .and_then(|snapshot| match &selector.purpose {
                AgentSelectorPurpose::Switch { session_id } => snapshot
                    .sessions
                    .current
                    .iter()
                    .find(|session| session.id == *session_id),
                AgentSelectorPurpose::Create => None,
            })
            .is_some_and(|session| session_active(session.status.phase));
        if active {
            "Active work continues with the current agent. The switch applies next cycle."
        } else {
            "The switch applies to the next cycle and preserves session history."
        }
    } else {
        "Choose the exact agent for the new session."
    };
    frame.render_widget(
        Paragraph::new(message)
            .style(THEME.panel_muted_style())
            .wrap(Wrap { trim: false }),
        message_area,
    );

    let mut lines = agent_catalog_lines(snapshot, selector, entries);
    if let Some(error) = if switching {
        section.agent_update.error()
    } else {
        section.create.error()
    } {
        lines.push(Line::raw(""));
        lines.push(ui::error_line(error));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        list_area,
    );
}

fn agent_catalog_lines(
    snapshot: Option<&ClientSnapshot>,
    selector: &AgentSelector,
    entries: &[SelectableAgentView],
) -> Vec<Line<'static>> {
    let Some(snapshot) = snapshot else {
        return vec![ui::loading_line("Loading agents...")];
    };
    if entries.is_empty() && matches!(snapshot.sessions.agents.load.status, "idle" | "loading") {
        return vec![ui::loading_line("Loading agents...")];
    }
    if let Some(error) = snapshot.sessions.agents.load.error.as_deref() {
        return vec![ui::error_line(error), ui::loading_line("Press r to retry.")];
    }
    if entries.is_empty() {
        return vec![
            ui::loading_line("No agents are available."),
            ui::loading_line("Press r to retry discovery."),
        ];
    }

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let selected = index == selector.selected;
            let style = if selected {
                THEME.focus_style().add_modifier(Modifier::BOLD)
            } else {
                THEME.text_style()
            };
            let mut spans = vec![
                Span::styled(if selected { "> " } else { "  " }, style),
                Span::styled(entry.name.clone(), style),
            ];
            if snapshot.sessions.agents.default_agent_name == entry.name {
                spans.push(Span::styled("  default", THEME.panel_muted_style()));
            }
            if let Some(description) = entry.description.as_deref() {
                spans.push(Span::styled(
                    format!("  {description}"),
                    THEME.panel_muted_style(),
                ));
            }
            Line::from(spans)
        })
        .collect()
}
