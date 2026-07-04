//! Splash screen rendering.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph},
};

use waltr_client::ClientSnapshot;

use crate::{theme::THEME, ui::centered_rect};

const WALTR_ASCII: &[&str] = &[
    "W     W    A    L      TTTTTTT RRRRR ",
    "W     W   A A   L         T    R    R",
    "W     W  A   A  L         T    R    R",
    "W  W  W AAAAAAA L         T    RRRRR ",
    "W W W W A     A L         T    R  R  ",
    "WW   WW A     A L         T    R   R ",
    "W     W A     A LLLLLLL   T    R    R",
];

/// Render the splash screen content.
pub fn draw_splash(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&ClientSnapshot>) {
    let card = centered_rect(area, 52, 11);

    frame.render_widget(Clear, card);
    frame.render_widget(splash_widget(snapshot), card);
}

fn splash_widget(snapshot: Option<&ClientSnapshot>) -> Paragraph<'static> {
    let mut lines = WALTR_ASCII
        .iter()
        .copied()
        .map(|line| Line::from(Span::styled(line, THEME.accent_style())))
        .collect::<Vec<_>>();

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        status_text(snapshot),
        THEME.panel_muted_style(),
    )));

    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .style(THEME.panel_style())
        .block(
            Block::default()
                .style(THEME.panel_style())
                .padding(Padding::new(2, 2, 1, 1)),
        )
}

fn status_text(snapshot: Option<&ClientSnapshot>) -> String {
    snapshot.map_or_else(
        || "waiting for server status...".to_owned(),
        |snapshot| {
            snapshot.status.watch.error.as_ref().map_or_else(
                || "waiting for server status...".to_owned(),
                |error| format!("status unavailable: {error}"),
            )
        },
    )
}
