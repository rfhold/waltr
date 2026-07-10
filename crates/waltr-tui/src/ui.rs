//! Global terminal UI composition.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    app::{App, AppScreen},
    screens,
    theme::{StatusRole, THEME},
};

/// Confirmation copy shared by destructive row prompts.
pub const DELETE_CONFIRMATION_HINT: &str = "Delete? Enter/y confirm, n/Esc cancel";

/// Red bottom-left status notice rendered by the global UI wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Notice<'a> {
    text: &'a str,
}

impl<'a> Notice<'a> {
    /// Create a notice with the given text.
    #[must_use]
    pub const fn new(text: &'a str) -> Self {
        Self { text }
    }
}

/// Render the full TUI, including global overlays.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    draw_wrapper(frame, app.notice(), |frame, area| match app.screen() {
        AppScreen::Waiting => screens::draw_splash(frame, area, app.client_snapshot()),
        AppScreen::Landing => draw_app_shell(frame, area, app, false, |frame, area| {
            screens::draw_landing(frame, area, true);
        }),
        AppScreen::Config => {
            draw_app_shell(frame, area, app, true, |frame, area| {
                screens::draw_landing(frame, area, false);
            });
            screens::draw_config(frame, area, app);
        }
        AppScreen::Sessions => {
            draw_app_shell(frame, area, app, true, |frame, area| {
                if !app.sessions_section().in_detail() {
                    screens::draw_landing(frame, area, false);
                }
                screens::draw_sessions(frame, area, app.sessions_section(), app.client_snapshot());
            });
        }
    });

    render_auth_modal(frame, app);
}

fn render_auth_modal(frame: &mut Frame<'_>, app: &App) {
    let snapshot = match app.client_snapshot() {
        Some(snapshot) => snapshot,
        None => return,
    };
    let requirement = match snapshot.auth.pending.first() {
        Some(requirement) => requirement,
        None => return,
    };
    let attempt = snapshot.auth.active_attempt.as_ref().filter(|attempt| {
        attempt.target.kind == requirement.target.kind
            && attempt.target.key == requirement.target.key
    });
    let area = centered_percent_rect(frame.area(), 78, if attempt.is_some() { 16 } else { 10 });
    let body = render_modal_frame(
        frame,
        area,
        "OAuth required",
        if attempt.is_some() {
            "type/paste URL or code · Enter complete · Esc cancel"
        } else {
            "Enter begin OAuth · Esc dismiss"
        },
    );

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Target: ", THEME.muted_text_style()),
            Span::styled(auth_target_label(&requirement.target), THEME.accent_bold()),
        ]),
        Line::from(Span::styled(
            requirement.message.clone(),
            THEME.accent_bold(),
        )),
    ];

    if let Some(attempt) = attempt {
        lines.push(Line::from(""));
        for message in &attempt.messages {
            lines.push(Line::from(Span::styled(
                message.clone(),
                THEME.muted_text_style(),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled("URL: ", THEME.muted_text_style()),
            Span::styled(attempt.auth_url.clone(), THEME.text_style()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Input: ", THEME.muted_text_style()),
            Span::styled(app.auth_input().to_owned(), THEME.text_style()),
        ]));
    } else if let Some(error) = snapshot.auth.begin.error.as_ref() {
        lines.push(Line::from(""));
        lines.push(error_line(error));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        body,
    );
}

fn auth_target_label(target: &waltr_client::ClientAuthTarget) -> String {
    if target.kind == "mcpServer" {
        format!("MCP server {}", target.key)
    } else {
        format!("provider {}", target.key)
    }
}

/// Render content inside the global UI wrapper.
pub fn draw_wrapper(
    frame: &mut Frame<'_>,
    notice: Option<Notice<'_>>,
    render_content: impl FnOnce(&mut Frame<'_>, Rect),
) {
    let area = frame.area();
    let content_area = global_content_area(area);

    frame.render_widget(Clear, area);
    render_content(frame, content_area);

    if let Some(notice) = notice {
        render_exit_notice(frame, content_area, notice);
    }
}

pub(crate) fn global_content_area(area: Rect) -> Rect {
    let horizontal_padding = area.width.min(2);
    let left_padding = if horizontal_padding == 0 { 0 } else { 1 };

    Rect::new(
        area.x.saturating_add(left_padding),
        area.y,
        area.width.saturating_sub(horizontal_padding),
        area.height.saturating_sub(1),
    )
}

fn render_exit_notice(frame: &mut Frame<'_>, area: Rect, notice: Notice<'_>) {
    if area.height == 0 {
        return;
    }

    let notice_width = (area.width / 2).max(1);
    let notice_area = Rect::new(area.x, area.y + area.height - 1, notice_width, 1);
    let notice_text = format!("{:<width$}", notice.text, width = notice_width.into());
    let notice = Paragraph::new(notice_text).style(THEME.danger_style());

    frame.render_widget(notice, notice_area);
}

/// Return a fixed-size rectangle centered inside `area`.
#[must_use]
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let [_, vertical, _] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height.min(area.height)),
        Constraint::Min(0),
    ])
    .areas(area);

    let [_, centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(width.min(area.width)),
        Constraint::Min(0),
    ])
    .areas(vertical);

    centered
}

/// Return a percentage-width rectangle with fixed height centered inside `area`.
#[must_use]
pub fn centered_percent_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let width = area.width.saturating_mul(percent_x).saturating_div(100);
    centered_rect(area, width, height)
}

/// Render the shared borderless modal frame and return its body area.
pub fn render_modal_frame(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    footer_hint: &'static str,
) -> Rect {
    let block = modal_frame_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [title_area, title_gap, body_area, footer_gap, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    render_modal_title(frame, title_area, title);
    render_panel_gap(frame, title_gap);
    render_panel_gap(frame, footer_gap);
    render_modal_footer(frame, footer_area, footer_hint);

    body_area
}

/// Fill an area with the shared panel background.
pub fn render_panel_gap(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Block::default().style(THEME.panel_style()), area);
}

/// Build a raised panel title line.
#[must_use]
pub fn panel_title_line(title: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled(title.to_owned(), THEME.accent_bold().bg(THEME.panel())),
    ])
    .style(THEME.panel_style())
}

/// Build a borderless modal surface with shared title and footer hint styling.
#[must_use]
pub fn modal_block(title: &'static str, footer_hint: &'static str) -> Block<'static> {
    let block = Block::default()
        .style(THEME.panel_style())
        .padding(Padding::new(2, 2, 1, 1))
        .title(Line::from(Span::styled(
            title.trim().to_owned(),
            THEME.accent_bold().bg(THEME.panel()),
        )));
    if footer_hint.is_empty() {
        block
    } else {
        block.title_bottom(footer_hint_line(footer_hint.trim()))
    }
}

/// Build a muted footer hint line.
#[must_use]
pub fn footer_hint_line(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(text, THEME.muted_text_style()))
}

fn modal_frame_block() -> Block<'static> {
    Block::default()
        .style(THEME.panel_style())
        .padding(Padding::new(3, 3, 1, 1))
}

fn render_modal_title(frame: &mut Frame<'_>, area: Rect, title: &'static str) {
    frame.render_widget(
        Paragraph::new(modal_title_line(title)).style(THEME.panel_style()),
        area,
    );
}

fn modal_title_line(title: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled(
            title.trim().to_owned(),
            THEME.accent_bold().bg(THEME.panel()),
        ),
    ])
    .style(THEME.panel_style())
}

fn render_modal_footer(frame: &mut Frame<'_>, area: Rect, footer_hint: &'static str) {
    frame.render_widget(
        Paragraph::new(modal_footer_line(footer_hint)).style(THEME.panel_style()),
        area,
    );
}

fn modal_footer_line(footer_hint: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled(footer_hint.trim().to_owned(), THEME.panel_muted_style()),
    ])
    .style(THEME.panel_style())
}

/// Build a tab strip label span.
#[must_use]
pub fn tab_label(label: &'static str, active: bool) -> Span<'static> {
    let style = if active {
        THEME.accent_bold().add_modifier(Modifier::UNDERLINED)
    } else {
        THEME.muted_text_style()
    };

    Span::styled(format!(" {label} "), style)
}

/// Build a muted loading/status line.
#[must_use]
pub fn loading_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), THEME.muted_text_style()))
}

/// Build an error line with the standard `error:` prefix.
#[must_use]
pub fn error_line(error: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("error: {error}"),
        THEME.danger_style(),
    ))
}

/// Build a form field row with a shared active marker and label/value styling.
#[must_use]
pub fn form_field_row(
    index: usize,
    active: usize,
    label: impl Into<String>,
    value: impl Into<String>,
) -> Line<'static> {
    form_field_row_styled(index, active, label, value, THEME.text_style())
}

/// Build a form field row with a custom value style.
#[must_use]
pub fn form_field_row_styled(
    index: usize,
    active: usize,
    label: impl Into<String>,
    value: impl Into<String>,
    value_style: Style,
) -> Line<'static> {
    Line::from(vec![
        active_marker(index == active, "*"),
        Span::raw(" "),
        Span::styled(label.into(), THEME.muted_text_style()),
        Span::raw("  "),
        Span::styled(value.into(), value_style),
    ])
}

/// Build a selected-row marker span.
#[must_use]
pub fn selection_marker(selected: bool) -> Span<'static> {
    active_marker(selected, ">")
}

/// Build a selected-row title span.
#[must_use]
pub fn selected_title(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), THEME.accent_bold())
}

/// Build the shared destructive confirmation/status text.
#[must_use]
pub fn delete_prompt_text(pending: bool, error: Option<&str>) -> String {
    if pending {
        "Deleting...".to_owned()
    } else if let Some(error) = error {
        format!("error: {error}")
    } else {
        DELETE_CONFIRMATION_HINT.to_owned()
    }
}

/// Build a destructive prompt/status span.
#[must_use]
pub fn delete_prompt_span(text: impl Into<String>) -> Span<'static> {
    Span::styled(
        text.into(),
        THEME.danger_style().add_modifier(Modifier::BOLD),
    )
}

/// Build a saving status line.
#[must_use]
pub fn saving_line() -> Line<'static> {
    Line::from(Span::styled(
        "Saving...",
        THEME
            .status_style(StatusRole::Saving)
            .add_modifier(Modifier::BOLD),
    ))
}

fn active_marker(active: bool, active_text: &'static str) -> Span<'static> {
    let marker = if active { active_text } else { " " };
    Span::styled(marker, THEME.accent_style())
}

fn draw_app_shell(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    show_back_hint: bool,
    render_content: impl FnOnce(&mut Frame<'_>, Rect),
) {
    let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);

    render_content(frame, body);
    render_shell_footer(frame, footer, app, show_back_hint);
}

fn render_shell_footer(frame: &mut Frame<'_>, area: Rect, app: &App, show_back_hint: bool) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let footer_status = app
        .sessions_section()
        .detail_footer_status(app.client_snapshot());

    if let Some(status) = footer_status {
        let style = if status.error {
            THEME.danger_style()
        } else {
            THEME.status_style(StatusRole::Loading)
        };
        let hint = if status.interruptible {
            if app.session_interrupt_armed() {
                "Esc again interrupt"
            } else {
                "Esc Esc interrupt"
            }
        } else {
            "Esc back"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(status.text, style),
                Span::raw("    "),
                Span::styled(hint, THEME.muted_text_style()),
            ]))
            .style(THEME.muted_text_style()),
            left,
        );
    } else {
        let global_hint = if show_back_hint {
            "Esc  back    Ctrl-X q  quit"
        } else {
            "Ctrl-X q  quit"
        };

        frame.render_widget(
            Paragraph::new(footer_hint_line(global_hint)).style(THEME.muted_text_style()),
            left,
        );
    }

    frame.render_widget(shell_identity(app), right);
}

fn shell_identity(app: &App) -> Paragraph<'static> {
    let status = app.client_snapshot().map_or("connecting", |snapshot| {
        if snapshot.status.ready {
            "ready"
        } else {
            snapshot.status.watch.status
        }
    });

    Paragraph::new(Line::from(vec![
        Span::styled("Waltr", THEME.accent_bold()),
        Span::raw("  "),
        Span::styled(status.to_owned(), THEME.success_style()),
    ]))
    .style(THEME.muted_text_style())
    .alignment(Alignment::Right)
}
