//! Splash screen rendering.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Padding, Paragraph},
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
    let card = centered_rect(area, 48, 9);

    frame.render_widget(splash_widget(snapshot), card);
}

fn splash_widget(snapshot: Option<&ClientSnapshot>) -> Paragraph<'static> {
    let mut lines = WALTR_ASCII
        .iter()
        .copied()
        .map(|line| Line::from(Span::styled(line, THEME.accent_style())))
        .collect::<Vec<_>>();

    lines.push(Line::from(""));
    lines.push(Line::from(status_text(snapshot)));

    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .style(THEME.surface_style())
        .block(
            Block::default()
                .style(THEME.surface_style())
                .padding(Padding::horizontal(2)),
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
