//! Waltr access login screen.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph, Wrap},
};

use crate::{app::App, theme::THEME, ui::centered_rect};

/// Render the native Waltr access login screen.
pub fn draw_access(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let card = centered_rect(area, 70, 16);
    frame.render_widget(Clear, card);
    let mut lines = vec![
        Line::from(Span::styled("Waltr sign in", THEME.accent_bold())),
        Line::from(""),
        Line::from(Span::styled(app.access_reason(), THEME.text_style())),
        Line::from(""),
        Line::from(vec![
            Span::styled("Status  ", THEME.muted_text_style()),
            Span::styled(app.native_login_status(), THEME.accent_bold()),
        ]),
    ];
    if let Some(url) = app.native_login_url() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Authorization URL",
            THEME.muted_text_style(),
        )));
        lines.push(Line::from(Span::styled(url.to_owned(), THEME.text_style())));
    }
    if let Some(remaining) = app.native_login_remaining() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Expires in {remaining}s"),
            THEME.muted_text_style(),
        )));
    }
    if let Some(error) = app.native_login_error() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            THEME.danger_style(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        app.access_hint(),
        THEME.muted_text_style(),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false })
            .style(THEME.panel_style())
            .block(
                Block::default()
                    .style(THEME.panel_style())
                    .padding(Padding::new(3, 3, 1, 1)),
            ),
        card,
    );
}
