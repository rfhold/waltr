//! Configuration surface shell: hosts config concepts as tabs.
//!
//! The shell renders the config modal overlay, a tab strip, and delegates the
//! active tab's content to its section module. It owns no provider-specific
//! state; the provider section module ([`providers::ProviderSection`]) owns the
//! provider list, selection, form, and delete interactions.

mod defaults;
mod mcp;
mod providers;

pub use defaults::RuntimeDefaultsSection;
pub use mcp::{McpForm, McpFormMode, McpSection};
pub use providers::{
    ModelForm, ModelFormMode, ProviderForm, ProviderFormMode, ProviderSection, SectionAction,
};

use waltr_client::ClientSnapshot;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::{app::App, theme::THEME, ui};

/// Active config tab.
///
/// Exactly one tab is active at a time and determines which section content the
/// surface renders. Each tab maps to a self-contained section module that owns
/// its own list, selection, form, and delete state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConfigTab {
    /// Provider configuration section.
    #[default]
    Providers,
    /// MCP server configuration section.
    Mcp,
    /// Runtime defaults configuration section.
    Defaults,
}

impl ConfigTab {
    /// The ordered set of tabs presented in the strip.
    pub const ALL: [Self; 3] = [Self::Providers, Self::Mcp, Self::Defaults];

    /// The tab's display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Mcp => "MCP Servers",
            Self::Defaults => "Defaults",
        }
    }

    /// The next tab in the strip, wrapping around.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Providers => Self::Mcp,
            Self::Mcp => Self::Defaults,
            Self::Defaults => Self::Providers,
        }
    }
}

/// Render the configuration surface.
pub fn draw_config(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let modal_area = ui::centered_percent_rect(area, 82, 18);
    frame.render_widget(Clear, modal_area);

    let block = ui::modal_block(
        " Configuration ",
        " Esc back  Tab switch  C-x c create  Right/Enter edit  Backspace/Delete delete ",
    )
    .border_style(THEME.accent_style());
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let snapshot = app.client_snapshot();
    let active_tab = app.active_config_tab();

    let [tabs_area, content_area, status_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(status_height(snapshot)),
        ])
        .areas(inner);

    frame.render_widget(tab_strip(active_tab), tabs_area);

    // Pending = no config data available, and no load or watch error has
    // occurred. While pending the surface replaces the active section content
    // with a loading indicator that carries the current watch status. Once
    // config is loaded the section content fills the area and no standalone
    // watch footer is shown; the footer renders only to surface an error.
    let pending = is_pending(snapshot);
    if pending {
        frame.render_widget(loading_widget(snapshot), content_area);
    } else if has_error(snapshot) {
        frame.render_widget(status_widget(snapshot), status_area);
    }

    let config = snapshot.and_then(|snapshot| snapshot.config.current.as_ref());
    let loaded = snapshot.is_some_and(|snapshot| snapshot.config.watch.status == "received");

    match active_tab {
        ConfigTab::Providers => {
            let section = app.provider_section();
            if !pending {
                section.render_content(frame, content_area, config, loaded);
            }
            section.render_overlay(frame, area);
        }
        ConfigTab::Mcp => {
            let section = app.mcp_section();
            if !pending {
                section.render_content(frame, content_area, config, loaded);
            }
            section.render_overlay(frame, area);
        }
        ConfigTab::Defaults => {
            let section = app.runtime_defaults_section();
            if !pending {
                section.render_content(frame, content_area, config, loaded);
            }
            section.render_overlay(frame, area);
        }
    }
}

/// Whether the config surface is waiting for data: no configuration is
/// available yet and no load or watch error has occurred.
fn is_pending(snapshot: Option<&ClientSnapshot>) -> bool {
    let has_config = snapshot.is_some_and(|snapshot| snapshot.config.current.is_some());
    let has_error = snapshot.is_some_and(|snapshot| {
        snapshot.config.load.error.is_some() || snapshot.config.watch.error.is_some()
    });
    !has_config && !has_error
}

/// Intermediary loading indicator shown while config is pending. It carries the
/// current watch status so the standalone watch footer can be suppressed.
fn loading_widget(snapshot: Option<&ClientSnapshot>) -> Paragraph<'static> {
    let watch = snapshot.map_or("idle", |snapshot| snapshot.config.watch.status);

    let lines = vec![
        ui::loading_line("Loading configuration..."),
        Line::from(vec![
            Span::styled("watch ", THEME.muted_text_style()),
            Span::styled(watch.to_owned(), THEME.focus_style()),
        ]),
    ];

    Paragraph::new(lines).style(THEME.text_style())
}

/// Whether the config surface has a load or watch error to surface.
fn has_error(snapshot: Option<&ClientSnapshot>) -> bool {
    snapshot.is_some_and(|snapshot| {
        snapshot.config.load.error.is_some() || snapshot.config.watch.error.is_some()
    })
}

fn status_height(snapshot: Option<&ClientSnapshot>) -> u16 {
    u16::from(has_error(snapshot))
}

/// Error footer shown only when a load or watch error is present. Once config
/// is loaded cleanly no footer renders; the section content fills the area.
fn status_widget(snapshot: Option<&ClientSnapshot>) -> Paragraph<'static> {
    let error = snapshot.and_then(|snapshot| {
        snapshot
            .config
            .load
            .error
            .as_ref()
            .or(snapshot.config.watch.error.as_ref())
    });

    let lines = error.map_or_else(Vec::new, |error| vec![ui::error_line(error)]);

    Paragraph::new(lines).style(THEME.text_style())
}

/// Render the tab strip as labeled segments above a full-width divider rule.
///
/// The active tab is bold/underlined in the accent color; inactive tabs are
/// dim. A horizontal rule under the labels visually anchors the strip as a tab
/// bar rather than a floating word.
fn tab_strip(active: ConfigTab) -> Paragraph<'static> {
    let mut spans = Vec::new();
    spans.push(Span::raw(" "));
    for tab in ConfigTab::ALL {
        spans.push(ui::tab_label(tab.label(), tab == active));
        spans.push(Span::raw(" "));
    }

    let rule = Line::from(Span::styled("─".repeat(240), THEME.dim_accent_style()));

    Paragraph::new(vec![Line::from(spans), rule]).style(THEME.text_style())
}
