//! Landing screen rendering.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{theme::THEME, ui};

/// Render the ready landing screen.
pub fn draw_landing(frame: &mut Frame<'_>, area: Rect, show_palette: bool) {
    let hero_area = if show_palette {
        let [hero, _, _] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(7),
            Constraint::Min(0),
        ])
        .areas(area);
        hero
    } else {
        area
    };
    let hero_card = ui::centered_rect(hero_area, 58, 8);

    frame.render_widget(hero_widget(), hero_card);

    if show_palette {
        frame.render_widget(palette_widget(), ui::centered_rect(area, 42, 7));
    }
}

fn hero_widget() -> Paragraph<'static> {
    let lines = vec![
        Line::from(Span::styled("Ready to configure Waltr", THEME.accent_bold())),
        Line::from(""),
        Line::from("Connect providers, tune local settings,"),
        Line::from("and keep the runtime in sync from one shell."),
    ];

    Paragraph::new(lines).alignment(Alignment::Center)
}

fn palette_widget() -> Paragraph<'static> {
    let lines = vec![
        Line::from(vec![
            Span::styled("Ctrl-X c", THEME.focus_style()),
            Span::raw("  open configuration"),
        ]),
        Line::from(vec![
            Span::styled("Ctrl-X s", THEME.focus_style()),
            Span::raw("  open sessions"),
        ]),
        Line::from(vec![
            Span::styled("Ctrl-X q", THEME.focus_style()),
            Span::raw("  quit Waltr"),
        ]),
    ];

    Paragraph::new(lines).alignment(Alignment::Left).block(
        ui::modal_block(" Commands ", "")
            .border_style(THEME.accent_style())
            .style(THEME.surface_style()),
    )
}
