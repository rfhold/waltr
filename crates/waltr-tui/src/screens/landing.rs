//! Landing screen rendering.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph},
};

use crate::{theme::THEME, ui};

/// Render the ready landing screen.
pub fn draw_landing(frame: &mut Frame<'_>, area: Rect, show_palette: bool, can_logout: bool) {
    let modal_height = if show_palette { 10 } else { 8 };
    let modal_area = ui::centered_rect(area, 60, modal_height);
    frame.render_widget(Clear, modal_area);

    let block = landing_modal_block();
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let footer_gap_height = u16::from(show_palette);
    let footer_height = u16::from(show_palette);
    let [title_area, title_gap, body_area, footer_gap, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(footer_gap_height),
        Constraint::Length(footer_height),
    ])
    .areas(inner);

    render_landing_title(frame, title_area);
    render_gap(frame, title_gap);
    frame.render_widget(landing_body_widget(), body_area);

    if show_palette {
        render_gap(frame, footer_gap);
        render_landing_footer(frame, footer_area, can_logout);
    }
}

fn landing_modal_block() -> Block<'static> {
    Block::default()
        .style(THEME.panel_style())
        .padding(Padding::new(3, 3, 1, 1))
}

fn render_landing_title(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(landing_title_line()).style(THEME.panel_style()),
        area,
    );
}

fn landing_title_line() -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled("Waltr", THEME.accent_bold().bg(THEME.panel())),
    ])
    .style(THEME.panel_style())
}

fn landing_body_widget() -> Paragraph<'static> {
    let lines = vec![
        Line::from(Span::styled("Ready to configure Waltr", THEME.accent_bold())),
        Line::from(""),
        Line::from("Connect providers, tune local settings,"),
        Line::from("and keep the runtime in sync from one shell."),
    ];

    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .style(THEME.panel_style())
}

fn render_landing_footer(frame: &mut Frame<'_>, area: Rect, can_logout: bool) {
    frame.render_widget(
        Paragraph::new(landing_footer_line(can_logout)).style(THEME.panel_style()),
        area,
    );
}

fn landing_footer_line(can_logout: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", THEME.panel_style()),
        Span::styled(
            if can_logout {
                "C-x c config  C-x s sessions  C-x l logout  C-x q quit"
            } else {
                "C-x c config  C-x s sessions  C-x q quit"
            },
            THEME.panel_muted_style(),
        ),
    ])
    .style(THEME.panel_style())
}

fn render_gap(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Block::default().style(THEME.panel_style()), area);
}
