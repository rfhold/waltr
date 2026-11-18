//! Waltr access login screen.

use qrcodegen::QrCode;
use ratatui::{
    Frame,
    layout::{Alignment, Margin, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph, Wrap},
};

use crate::{app::App, event::DeviceLoginPrompt, theme::THEME, ui::centered_rect};

const QR_QUIET_ZONE: i32 = 4;
const QR_DARK: Color = Color::Rgb(0, 0, 0);
const QR_LIGHT: Color = Color::Rgb(255, 255, 255);

/// Render the native Waltr access login screen.
pub fn draw_access(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(prompt) = app.device_login_prompt() {
        draw_device(frame, area, app, prompt);
        return;
    }
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
    if card.height < 12 {
        lines = vec![lines.remove(0)];
        lines.push(Line::from(app.access_hint()));
        lines.push(Line::from(app.native_login_status()));
    }
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
    if card.height >= 12 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            app.access_hint(),
            THEME.muted_text_style(),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false })
            .style(THEME.panel_style())
            .block(Block::default().style(THEME.panel_style()).padding(
                if card.width < 36 || card.height < 12 {
                    Padding::ZERO
                } else {
                    Padding::new(3, 3, 1, 1)
                },
            )),
        card,
    );
}

fn draw_device(frame: &mut Frame<'_>, mut area: Rect, app: &App, prompt: &DeviceLoginPrompt) {
    // Keep clipboard notices off the QR and its quiet zone.
    area.height = area.height.saturating_sub(1);
    let mut lines = vec![
        Line::from(Span::styled("Waltr sign in", THEME.accent_bold())),
        Line::from(vec![
            Span::raw("Code: "),
            Span::styled(prompt.user_code.as_str(), THEME.accent_bold()),
        ]),
        Line::from(Span::styled(
            "c copy URL  b browser",
            THEME.muted_text_style(),
        )),
        Line::from(Span::styled("Esc cancel", THEME.muted_text_style())),
        Line::from("Open manually:"),
        Line::from(prompt.verification_uri.as_str()),
    ];
    if let Some(remaining) = app.native_login_remaining() {
        lines.push(Line::from(format!("Expires in {remaining}s")));
    }
    if area.width == 0 || area.height == 0 {
        return;
    }
    if let Some(qr) = &prompt.qr {
        let mut qr_lines = lines.clone();
        qr_lines.insert(1, Line::from("Scan to sign in"));
        let text = Paragraph::new(qr_lines)
            .wrap(Wrap { trim: false })
            .style(THEME.panel_style());
        let side = u16::try_from(qr.size() + QR_QUIET_ZONE * 2).expect("QR size <= 185");
        let rows = side.div_ceil(2);
        let width = area.width.min(76.max(side + 36));
        if let Some(text_width) = width.checked_sub(side + 2).filter(|width| *width >= 22) {
            let text_rows = u16::try_from(text.line_count(text_width)).unwrap_or(u16::MAX);
            let height = rows.max(text_rows);
            if height <= area.height {
                let card = centered_rect(area, width, height);
                let qr_area = Rect::new(card.x, card.y, side, rows);
                draw_qr(frame, qr_area, qr);
                let text_area = Rect::new(card.x + side + 2, card.y, text_width, height);
                frame.render_widget(text, text_area);
                return;
            }
        }
        let text_rows = u16::try_from(text.line_count(width)).unwrap_or(u16::MAX);
        let height = rows.saturating_add(text_rows);
        if side <= width && height <= area.height {
            let card = centered_rect(area, width, height);
            let text_area = Rect::new(card.x, card.y, width, text_rows);
            frame.render_widget(text, text_area);
            draw_qr(
                frame,
                Rect::new(card.x + (width - side) / 2, card.y + text_rows, side, rows),
                qr,
            );
            return;
        }
    }

    let card = centered_rect(area, 70, 12);
    let card = if card.width >= 36 && card.height >= 9 {
        card.inner(Margin {
            horizontal: 3,
            vertical: 1,
        })
    } else {
        card
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(THEME.panel_style()),
        card,
    );
}

fn draw_qr(frame: &mut Frame<'_>, area: Rect, qr: &QrCode) {
    let mut lines = Vec::with_capacity(area.height.into());
    for row in 0..area.height {
        let y = i32::from(row) * 2 - QR_QUIET_ZONE;
        let spans = (0..area.width).map(|column| {
            let x = i32::from(column) - QR_QUIET_ZONE;
            let top = qr.get_module(x, y);
            let bottom = qr.get_module(x, y + 1);
            let fg = if top { QR_DARK } else { QR_LIGHT };
            let bg = if bottom { QR_DARK } else { QR_LIGHT };
            Span::styled(
                if top == bottom { " " } else { "▀" },
                Style::default().fg(fg).bg(bg),
            )
        });
        lines.push(Line::from_iter(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}
