//! Splash screen rendering.

use ratatui::{
    Frame,
    layout::Alignment,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use waltr_client::ClientSnapshot;

use crate::ui::centered_rect;

const LOGO_COLOR: Color = Color::Rgb(190, 132, 242);

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
pub fn draw_splash(frame: &mut Frame<'_>, snapshot: Option<&ClientSnapshot>) {
    let area = frame.area();
    let card = centered_rect(area, 48, 9);

    frame.render_widget(splash_widget(snapshot), card);
}

fn splash_widget(snapshot: Option<&ClientSnapshot>) -> Paragraph<'static> {
    let logo_style = Style::default().fg(LOGO_COLOR);
    let mut lines = WALTR_ASCII
        .iter()
        .copied()
        .map(|line| Line::from(Span::styled(line, logo_style)))
        .collect::<Vec<_>>();

    lines.push(Line::from(""));
    lines.push(Line::from(status_text(snapshot)));

    Paragraph::new(lines).alignment(Alignment::Center)
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
