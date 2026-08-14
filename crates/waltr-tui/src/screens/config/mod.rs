//! Configuration surface shell: hosts config concepts as tabs.
//!
//! The shell renders the config modal overlay, a tab strip, and delegates the
//! active tab's content to its section module. It owns no provider-specific
//! state; the provider section module ([`providers::ProviderSection`]) owns the
//! provider list, selection, form, and delete interactions.

mod auth;
mod inference_defaults;
mod mcp;
mod providers;

pub use auth::{ConfigAuthState, targets_match};
pub use inference_defaults::InferenceDefaultsSection;
pub use mcp::{McpForm, McpFormMode, McpSection};
pub use providers::{ModelForm, ModelFormMode, ProviderForm, ProviderFormMode, ProviderSection};

use waltr_client::{ClientCommand, ClientOperationView, ClientSnapshot};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    app::App,
    screens::{OperationResult, OperationTracker, SectionAction},
    theme::THEME,
    ui,
};

/// Config-shell state for resetting every configuration section.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigResetState {
    open: bool,
    operation: OperationTracker,
}

impl ConfigResetState {
    pub(crate) const fn new() -> Self {
        Self {
            open: false,
            operation: OperationTracker::new(),
        }
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) const fn is_pending(&self) -> bool {
        self.operation.is_pending()
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.operation.error()
    }

    pub(crate) fn open(&mut self) {
        self.open = true;
        self.operation.clear_error();
    }

    pub(crate) fn dismiss(&mut self) -> bool {
        if !self.open || self.operation.is_pending() {
            return false;
        }
        self.open = false;
        self.operation.clear();
        true
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> SectionAction {
        if !self.open || self.operation.is_pending() {
            return SectionAction::Continue;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                self.operation.begin();
                SectionAction::ClientCommand(ClientCommand::ResetConfig)
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.dismiss();
                SectionAction::Continue
            }
            _ => SectionAction::Continue,
        }
    }

    pub(crate) fn reconcile(&mut self, snapshot: &ClientSnapshot) -> bool {
        let result = self
            .operation
            .reconcile(&snapshot.config.reset, snapshot, |operation| {
                let ClientOperationView::ConfigReset { status, error, .. } = operation else {
                    return None;
                };
                Some(match *status {
                    "ready" => OperationResult::Ready(()),
                    "failed" => OperationResult::Failed(
                        error
                            .clone()
                            .unwrap_or_else(|| "configuration reset failed".to_owned()),
                    ),
                    _ => OperationResult::Pending,
                })
            });
        if result.as_ref().is_some_and(Result::is_ok) {
            self.open = false;
            return true;
        }
        false
    }
}

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
    /// Inference defaults configuration section.
    InferenceDefaults,
}

impl ConfigTab {
    /// The ordered set of tabs presented in the strip.
    pub const ALL: [Self; 3] = [Self::Providers, Self::Mcp, Self::InferenceDefaults];

    /// The tab's display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Mcp => "MCP Servers",
            Self::InferenceDefaults => "Inference",
        }
    }

    /// The next tab in the strip, wrapping around.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Providers => Self::Mcp,
            Self::Mcp => Self::InferenceDefaults,
            Self::InferenceDefaults => Self::Providers,
        }
    }
}

/// Render the configuration surface.
pub fn draw_config(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let modal_area = ui::centered_percent_rect(area, 90, 20);
    frame.render_widget(Clear, modal_area);

    let snapshot = app.client_snapshot();
    let active_tab = app.active_config_tab();
    let body_area = render_modal_frame(
        frame,
        modal_area,
        "Configuration",
        config_footer_hint(active_tab),
    );

    let [tabs_area, tabs_gap, content_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(status_height(snapshot)),
    ])
    .areas(body_area);

    let config_loaded = snapshot.is_some_and(|snapshot| snapshot.config.current.is_some());
    frame.render_widget(tab_strip(active_tab, config_loaded), tabs_area);
    ui::render_panel_gap(frame, tabs_gap);

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
    let statuses =
        snapshot.map_or_else(Default::default, |snapshot| snapshot.auth.statuses.clone());
    let loaded = snapshot.is_some_and(|snapshot| snapshot.config.watch.status == "received");

    match active_tab {
        ConfigTab::Providers => {
            let section = app.provider_section();
            if !pending {
                section.render_content(
                    frame,
                    content_area,
                    config,
                    loaded,
                    &statuses,
                    app.config_auth_state(),
                );
            }
            section.render_overlay(frame, modal_area);
        }
        ConfigTab::Mcp => {
            let section = app.mcp_section();
            if !pending {
                section.render_content(
                    frame,
                    content_area,
                    config,
                    loaded,
                    &statuses,
                    app.config_auth_state(),
                );
            }
            section.render_overlay(frame, modal_area);
        }
        ConfigTab::InferenceDefaults => {
            let section = app.inference_defaults_section();
            if !pending {
                section.render_content(frame, content_area, config, loaded);
            }
            section.render_overlay(frame, modal_area, config);
        }
    }

    render_reset_overlay(frame, modal_area, app);
}

const fn config_footer_hint(active_tab: ConfigTab) -> &'static str {
    match active_tab {
        ConfigTab::Providers | ConfigTab::Mcp => {
            "Esc back Tab switch C-x c create Enter edit Del delete a/f/r"
        }
        ConfigTab::InferenceDefaults => {
            "Esc back  Tab switch  m model  r reason  Enter save  Del clear"
        }
    }
}

fn render_modal_frame(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    footer_hint: &'static str,
) -> Rect {
    ui::render_modal_frame(frame, area, title, footer_hint)
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

    Paragraph::new(lines).style(THEME.panel_style())
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

    Paragraph::new(lines).style(THEME.panel_style())
}

/// Render the tab strip as labeled segments on the modal surface.
fn tab_strip(active: ConfigTab, config_loaded: bool) -> Paragraph<'static> {
    let mut spans = Vec::new();
    spans.push(Span::raw(" "));
    for tab in ConfigTab::ALL {
        spans.push(ui::tab_label(tab.label(), tab == active));
        spans.push(Span::raw(" "));
    }
    if config_loaded {
        spans.push(Span::styled("R Reset configuration", THEME.focus_style()));
    }

    Paragraph::new(Line::from(spans)).style(THEME.panel_style())
}

fn render_reset_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if !app.config_reset_open() {
        return;
    }

    let reset_area = ui::centered_percent_rect(area, 82, 13);
    frame.render_widget(Clear, reset_area);
    let footer = if app.config_reset_pending() {
        "Reset in progress"
    } else if app.config_reset_error().is_some() {
        "Enter/y retry  n/Esc cancel"
    } else {
        "Enter/y confirm  n/Esc cancel"
    };
    let body = render_modal_frame(frame, reset_area, "Reset configuration", footer);
    let mut lines = vec![
        Line::from("Providers, MCP servers, and inference defaults will return"),
        Line::from("to server defaults. Credentials remain untouched."),
        Line::from(""),
    ];
    if app.config_reset_pending() {
        lines.push(ui::loading_line("Resetting configuration..."));
    } else {
        lines.push(Line::from(Span::styled(
            "This replaces all current configuration sections.",
            THEME.muted_text_style(),
        )));
    }
    if let Some(error) = app.config_reset_error() {
        lines.push(ui::error_line(error));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        body,
    );
}
