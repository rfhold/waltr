//! Self-contained MCP server config section: state, input, and rendering.
//!
//! This module owns the MCP server list selection, the create/edit form, and
//! the delete confirmation. It is hosted by the config surface shell behind the
//! "MCP Servers" tab and does not depend on the surrounding tab chrome. The
//! shell holds no MCP-specific state; all of it lives here.

use waltr_client::{ClientCommand, ConfigView, McpServerTypeView, McpServerView};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{theme::THEME, ui};

use super::SectionAction;

/// MCP server form mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpFormMode {
    /// Create a new MCP server.
    Create,
    /// Edit the selected MCP server.
    Edit,
}

/// Active MCP server form state.
///
/// The server key is its `name`, mirroring how providers key on their name;
/// there is no separate key field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpForm {
    /// Form mode.
    pub mode: McpFormMode,
    /// Active field index.
    pub field: usize,
    /// Original server key/name for edits.
    pub original_name: Option<String>,
    /// Server key/name draft.
    pub name: String,
    /// Server type draft.
    pub kind: McpServerTypeView,
    /// Launch command draft (stdio servers).
    pub command: String,
    /// Endpoint URL draft (sse servers).
    pub url: String,
}

const MCP_FIELD_COUNT: usize = 3;

impl McpForm {
    const fn create() -> Self {
        Self {
            mode: McpFormMode::Create,
            field: 0,
            original_name: None,
            name: String::new(),
            kind: McpServerTypeView::Stdio,
            command: String::new(),
            url: String::new(),
        }
    }

    fn edit(key: &str, server: &McpServerView) -> Self {
        Self {
            mode: McpFormMode::Edit,
            field: 0,
            original_name: Some(key.to_owned()),
            name: server.name.clone(),
            kind: server.kind,
            command: server.command.clone(),
            url: server.url.clone(),
        }
    }

    /// Whether the type-required conditional field is filled, gating save.
    ///
    /// Stdio servers require a `command`; sse servers require a `url`. Other
    /// types cannot be saved from this form.
    #[must_use]
    pub fn type_field_filled(&self) -> bool {
        match self.kind {
            McpServerTypeView::Stdio => !self.command.trim().is_empty(),
            McpServerTypeView::Sse => !self.url.trim().is_empty(),
            McpServerTypeView::Unspecified | McpServerTypeView::Unknown => false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        match key.code {
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % MCP_FIELD_COUNT,
            KeyCode::BackTab | KeyCode::Up => {
                self.field = (self.field + MCP_FIELD_COUNT - 1) % MCP_FIELD_COUNT;
            }
            KeyCode::Left | KeyCode::Right if self.field == 1 => self.toggle_kind(),
            KeyCode::Backspace if self.is_text_field() => drop(self.active_text_mut().pop()),
            KeyCode::Enter => return FormAction::Submit,
            KeyCode::Char(character) if self.is_text_field() => {
                self.active_text_mut().push(character);
            }
            _ => {}
        }

        FormAction::Continue
    }

    const fn is_text_field(&self) -> bool {
        matches!(self.field, 0 | 2)
    }

    /// The conditional field at index 2 follows the server type: `command` for
    /// stdio, `url` for sse.
    const fn active_text_mut(&mut self) -> &mut String {
        match self.field {
            2 => match self.kind {
                McpServerTypeView::Sse => &mut self.url,
                _ => &mut self.command,
            },
            _ => &mut self.name,
        }
    }

    const fn toggle_kind(&mut self) {
        self.kind = match self.kind {
            McpServerTypeView::Stdio => McpServerTypeView::Sse,
            _ => McpServerTypeView::Stdio,
        };
    }

    /// Build the server view, keeping only the field relevant to the type.
    fn server(&self) -> McpServerView {
        let (command, url) = match self.kind {
            McpServerTypeView::Sse => (String::new(), self.url.clone()),
            _ => (self.command.clone(), String::new()),
        };

        McpServerView {
            name: self.name.clone(),
            kind: self.kind,
            command,
            url,
        }
    }
}

enum FormAction {
    Continue,
    Submit,
}

/// Self-contained MCP server config section state and behavior.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpSection {
    config_form: Option<McpForm>,
    delete_server: Option<String>,
    selected_server: usize,
    pending_patch: bool,
    patch_error: Option<String>,
}

impl McpSection {
    /// Create an empty MCP section.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config_form: None,
            delete_server: None,
            selected_server: 0,
            pending_patch: false,
            patch_error: None,
        }
    }

    /// Return the selected server index.
    #[must_use]
    pub const fn selected_server(&self) -> usize {
        self.selected_server
    }

    /// Return the active MCP form, if any.
    #[must_use]
    pub const fn config_form(&self) -> Option<&McpForm> {
        self.config_form.as_ref()
    }

    /// Return the server pending delete confirmation, if any.
    #[must_use]
    pub fn delete_server(&self) -> Option<&str> {
        self.delete_server.as_deref()
    }

    /// Whether a transient overlay (form or delete prompt) is active.
    #[must_use]
    pub const fn has_overlay(&self) -> bool {
        self.config_form.is_some() || self.delete_server.is_some()
    }

    /// Reconcile a submitted patch against the shared reducer operation state.
    pub fn reconcile_patch(&mut self, status: &str, error: Option<&str>) {
        if !self.pending_patch {
            return;
        }

        match status {
            "ready" => {
                self.pending_patch = false;
                self.patch_error = None;
                self.config_form = None;
                self.delete_server = None;
            }
            "failed" => {
                self.pending_patch = false;
                self.patch_error = Some(error.unwrap_or("config patch failed").to_owned());
            }
            _ => {}
        }
    }

    /// Clamp the selection to the available servers after a snapshot update.
    pub const fn sync_selection(&mut self, server_count: usize) {
        if server_count == 0 {
            self.selected_server = 0;
        } else if self.selected_server >= server_count {
            self.selected_server = server_count - 1;
        }
    }

    /// Dismiss any active overlay. Returns true if something was dismissed.
    pub fn dismiss_overlay(&mut self) -> bool {
        if self.pending_patch {
            return false;
        }

        let dismissed = self.config_form.take().is_some() || self.delete_server.take().is_some();
        if dismissed {
            self.patch_error = None;
        }
        dismissed
    }

    /// Open the MCP server create form.
    pub fn open_create_form(&mut self) {
        if self.pending_patch {
            return;
        }

        self.delete_server = None;
        self.patch_error = None;
        self.config_form = Some(McpForm::create());
    }

    /// Open the MCP server edit form for the current selection.
    pub fn open_edit_form(&mut self, config: Option<&ConfigView>) {
        if self.pending_patch {
            return;
        }

        self.delete_server = None;
        self.patch_error = None;
        self.config_form = self
            .selected_server_view(config)
            .map(|(key, server)| McpForm::edit(&key, server));
    }

    /// Arm the delete confirmation for the current selection.
    pub fn arm_delete(&mut self, config: Option<&ConfigView>) {
        if self.pending_patch {
            return;
        }

        self.config_form = None;
        self.patch_error = None;
        self.delete_server = self.selected_server_key(config);
    }

    /// Route a key event into the section, using the current config snapshot.
    pub fn handle_key(&mut self, key: KeyEvent, config: Option<&ConfigView>) -> SectionAction {
        if self.pending_patch {
            return SectionAction::Continue;
        }

        if let Some(form) = &mut self.config_form {
            return match form.handle_key(key) {
                FormAction::Continue => SectionAction::Continue,
                FormAction::Submit => self.submit_form(),
            };
        }

        if let Some(name) = self.delete_server.clone() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.delete_selected(&name),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.delete_server = None;
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            };
        }

        match key.code {
            KeyCode::Up => {
                self.selected_server = self.selected_server.saturating_sub(1);
                SectionAction::Continue
            }
            KeyCode::Down => {
                let max = server_count(config).saturating_sub(1);
                self.selected_server = (self.selected_server + 1).min(max);
                SectionAction::Continue
            }
            KeyCode::Right | KeyCode::Enter => {
                self.open_edit_form(config);
                SectionAction::Continue
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.arm_delete(config);
                SectionAction::Continue
            }
            _ => SectionAction::Unhandled,
        }
    }

    fn submit_form(&mut self) -> SectionAction {
        let Some(form) = &self.config_form else {
            return SectionAction::Continue;
        };

        // Block saving until the type-required field is provided, keeping the
        // form open so the missing-field indicator stays visible.
        if !form.type_field_filled() {
            return SectionAction::Continue;
        }

        let mode = form.mode;
        let original_name = form.original_name.clone();
        let server = form.server();
        self.pending_patch = true;
        self.patch_error = None;

        match mode {
            McpFormMode::Create => {
                SectionAction::ClientCommand(ClientCommand::AddMcpServer { server })
            }
            McpFormMode::Edit => {
                let Some(original_name) = original_name else {
                    return SectionAction::Continue;
                };
                SectionAction::ClientCommand(ClientCommand::UpdateMcpServer {
                    original_name,
                    server,
                })
            }
        }
    }

    fn delete_selected(&mut self, name: &str) -> SectionAction {
        self.pending_patch = true;
        self.patch_error = None;
        SectionAction::ClientCommand(ClientCommand::DeleteMcpServer {
            name: name.to_owned(),
        })
    }

    fn selected_server_view<'a>(
        &self,
        config: Option<&'a ConfigView>,
    ) -> Option<(String, &'a McpServerView)> {
        config?
            .mcp_servers
            .iter()
            .nth(self.selected_server)
            .map(|(key, server)| (key.clone(), server))
    }

    fn selected_server_key(&self, config: Option<&ConfigView>) -> Option<String> {
        config?
            .mcp_servers
            .keys()
            .nth(self.selected_server)
            .cloned()
    }

    /// Render the section content (MCP server list) into `area`.
    ///
    /// `loaded` indicates the watch stream has delivered config; when false the
    /// section shows a waiting placeholder.
    pub fn render_content(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        config: Option<&ConfigView>,
        loaded: bool,
    ) {
        let lines = if loaded {
            config.map_or_else(loading_lines, |config| {
                config_lines(
                    config,
                    self.selected_server,
                    self.delete_server(),
                    self.pending_patch,
                    self.patch_error.as_deref(),
                )
            })
        } else {
            loading_lines()
        };

        frame.render_widget(
            Paragraph::new(lines)
                .style(THEME.text_style())
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    /// Render the create/edit form overlay, if active.
    pub fn render_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(form) = &self.config_form {
            let form_area = ui::centered_percent_rect(area, 70, 10);
            frame.render_widget(Clear, form_area);
            frame.render_widget(
                form_widget(form, self.pending_patch, self.patch_error.as_deref()),
                form_area,
            );
        }
    }
}

fn server_count(config: Option<&ConfigView>) -> usize {
    config.map_or(0, |config| config.mcp_servers.len())
}

fn loading_lines() -> Vec<Line<'static>> {
    vec![ui::loading_line("Waiting for configuration...")]
}

fn config_lines(
    config: &ConfigView,
    selected_server: usize,
    delete_server: Option<&str>,
    pending_patch: bool,
    patch_error: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if config.mcp_servers.is_empty() {
        lines.push(ui::loading_line("No MCP servers configured."));
        return lines;
    }

    for (index, (key, server)) in config.mcp_servers.iter().enumerate() {
        lines.push(server_line(
            key,
            server,
            ServerLineState {
                selected: index == selected_server,
                deleting: delete_server == Some(key.as_str()),
                pending_patch,
                patch_error,
            },
        ));
    }

    lines
}

#[derive(Clone, Copy)]
struct ServerLineState<'a> {
    selected: bool,
    deleting: bool,
    pending_patch: bool,
    patch_error: Option<&'a str>,
}

fn server_line(key: &str, server: &McpServerView, state: ServerLineState<'_>) -> Line<'static> {
    let display = if server.name.is_empty() {
        key.to_owned()
    } else {
        server.name.clone()
    };

    if state.deleting {
        return Line::from(vec![
            ui::selection_marker(state.selected),
            Span::raw(" "),
            ui::selected_title(display),
            Span::raw("  "),
            ui::delete_prompt_span(ui::delete_prompt_text(
                state.pending_patch,
                state.patch_error,
            )),
        ]);
    }

    Line::from(vec![
        ui::selection_marker(state.selected),
        Span::raw(" "),
        ui::selected_title(display),
        Span::raw("  "),
        Span::styled(mcp_type_name(server.kind), THEME.focus_style()),
        Span::raw("  "),
        Span::styled(server_target(server), THEME.text_style()),
    ])
}

fn server_target(server: &McpServerView) -> String {
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

fn form_widget(
    form: &McpForm,
    pending_patch: bool,
    patch_error: Option<&str>,
) -> Paragraph<'static> {
    let title = match form.mode {
        McpFormMode::Create => " Create MCP server ",
        McpFormMode::Edit => " Edit MCP server ",
    };

    Paragraph::new(form_lines(form, pending_patch, patch_error))
        .block(
            ui::modal_block(
                title,
                " Tab field  Left/Right type  Enter save  Esc cancel ",
            )
            .style(THEME.text_style()),
        )
        .style(THEME.text_style())
        .wrap(Wrap { trim: false })
}

fn form_lines(
    form: &McpForm,
    pending_patch: bool,
    patch_error: Option<&str>,
) -> Vec<Line<'static>> {
    // The conditional field swaps label and value with the server type: stdio
    // shows `command`, sse shows `url`.
    let (conditional_label, conditional_value) = match form.kind {
        McpServerTypeView::Sse => ("url", form.url.as_str()),
        _ => ("command", form.command.as_str()),
    };

    let mut lines = vec![
        form_line(0, form.field, "name", &form.name),
        form_line(1, form.field, "type", mcp_type_name(form.kind)),
        form_line(2, form.field, conditional_label, conditional_value),
    ];

    if !form.type_field_filled() {
        lines.push(Line::from(Span::styled(
            format!("{conditional_label} required before saving"),
            THEME.danger_style().add_modifier(Modifier::BOLD),
        )));
    }

    if pending_patch {
        lines.push(ui::saving_line());
    } else if let Some(error) = patch_error {
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            THEME.danger_style().add_modifier(Modifier::BOLD),
        )));
    }

    lines
}

fn form_line(index: usize, active: usize, label: &str, value: &str) -> Line<'static> {
    ui::form_field_row(index, active, label, value)
}

const fn mcp_type_name(kind: McpServerTypeView) -> &'static str {
    match kind {
        McpServerTypeView::Unspecified => "unspecified",
        McpServerTypeView::Stdio => "stdio",
        McpServerTypeView::Sse => "sse",
        McpServerTypeView::Unknown => "unknown",
    }
}
