//! Detail composer rendering and cursor helpers.

use waltr_client::{
    ClientSnapshot, SessionRuntimeConfigView, SessionTemplateSearchSnapshot, SessionView,
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};

use crate::theme::{StatusRole, THEME};

use super::{
    DETAIL_COMPOSER_MAX_INPUT_LINES, SessionsSection, cache_rate, format_compact_currency_cost,
    format_count, panel_muted_style, panel_style, template_style::template_inline_spans,
    timeline::accent_border_span,
};

const RESOURCE_AUTOCOMPLETE_MAX_ROWS: u16 = 6;

pub(super) fn composer_visible_input_line_count(text: &str) -> u16 {
    u16::try_from(composer_line_count(text))
        .unwrap_or(DETAIL_COMPOSER_MAX_INPUT_LINES)
        .clamp(1, DETAIL_COMPOSER_MAX_INPUT_LINES)
}

pub(super) fn composer_line_count(text: &str) -> usize {
    text.split('\n').count().max(1)
}

fn composer_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    text.split('\n').map(str::to_owned).collect()
}

pub(super) fn composer_cursor_line_column(text: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(text.len());
    let mut line = 0usize;
    let mut column = 0usize;
    for character in text[..cursor].chars() {
        if character == '\n' {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    (line, column)
}

pub(super) fn composer_line_start(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

pub(super) fn composer_line_end(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index)
}

pub(super) fn composer_cursor_at_line_column(
    text: &str,
    target_line: usize,
    target_column: usize,
) -> usize {
    let mut line = 0usize;
    let mut column = 0usize;
    for (index, character) in text.char_indices() {
        if line == target_line && column == target_column {
            return index;
        }
        if character == '\n' {
            if line == target_line {
                return index;
            }
            line += 1;
            column = 0;
        } else if line == target_line {
            column += 1;
        }
    }
    text.len()
}

pub(super) fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
    session: &SessionView,
    context_tokens: u64,
) {
    frame.render_widget(Block::default().style(panel_style()), area);

    if area.width == 0 || area.height == 0 {
        return;
    }

    let [border_area, inner_area, right_padding_area] = Layout::horizontal([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let border_lines = (0..border_area.height)
        .map(|_| Line::from(accent_border_span()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(border_lines).style(panel_style()),
        border_area,
    );
    frame.render_widget(
        Paragraph::new(Line::default()).style(panel_style()),
        right_padding_area,
    );

    let input_height = area.height.saturating_sub(4).max(1);
    let [
        top_padding,
        input_area,
        spacer_area,
        metadata_area,
        bottom_padding_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner_area);

    frame.render_widget(
        Paragraph::new(Line::default()).style(panel_style()),
        top_padding,
    );
    let (input_widget, cursor_position) = composer_input_widget(section, input_area);
    frame.render_widget(input_widget, input_area);
    frame.render_widget(
        Paragraph::new(Line::default()).style(panel_style()),
        spacer_area,
    );
    render_composer_metadata(frame, metadata_area, session, context_tokens);
    frame.render_widget(
        Paragraph::new(Line::default()).style(panel_style()),
        bottom_padding_area,
    );
    render_resource_autocomplete_overlay(frame, area, inner_area, section, snapshot);
    frame.set_cursor_position(cursor_position);
}

fn render_resource_autocomplete_overlay(
    frame: &mut Frame<'_>,
    composer_area: Rect,
    inner_area: Rect,
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) {
    if !section.resource_autocomplete.open || composer_area.y <= 1 || inner_area.width <= 2 {
        return;
    }

    let height = resource_autocomplete_overlay_height(section, snapshot)
        .min(composer_area.y.saturating_sub(1))
        .min(RESOURCE_AUTOCOMPLETE_MAX_ROWS.saturating_add(2));
    if height <= 2 {
        return;
    };

    let area = Rect {
        x: inner_area.x,
        y: composer_area.y.saturating_sub(height).saturating_sub(1),
        width: inner_area.width,
        height,
    };
    let body_area = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let lines = match section.current_template_search(snapshot) {
        Some(search) => resource_autocomplete_lines(section, search, usize::from(body_area.height)),
        None => vec![Line::from(vec![Span::styled(
            "Searching resources...",
            panel_muted_style(),
        )])],
    };

    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(Line::default()).style(panel_style()), area);
    frame.render_widget(Paragraph::new(lines).style(panel_style()), body_area);
}

fn resource_autocomplete_overlay_height(
    section: &SessionsSection,
    snapshot: Option<&ClientSnapshot>,
) -> u16 {
    let rows = section
        .current_template_search(snapshot)
        .map_or(1, |search| match search.status {
            "loading" => 1,
            _ if search.error.is_some() || search.options.is_empty() => 1,
            _ => u16::try_from(search.options.len())
                .unwrap_or(RESOURCE_AUTOCOMPLETE_MAX_ROWS)
                .min(RESOURCE_AUTOCOMPLETE_MAX_ROWS),
        });

    rows.saturating_add(2)
}

fn resource_autocomplete_lines(
    section: &SessionsSection,
    search: &SessionTemplateSearchSnapshot,
    max_lines: usize,
) -> Vec<Line<'static>> {
    if search.status == "loading" {
        return vec![Line::from(vec![
            Span::raw("  "),
            Span::styled("Searching resources...", panel_muted_style()),
        ])];
    }
    if let Some(error) = &search.error {
        return vec![Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("Resource search failed: {error}"),
                THEME.status_style(StatusRole::Error),
            ),
        ])];
    }
    if search.options.is_empty() {
        return vec![Line::from(vec![
            Span::raw("  "),
            Span::styled("No resources found", panel_muted_style()),
        ])];
    }

    let selected = section.selected_resource_index(search.options.len());
    let start = selected.saturating_add(1).saturating_sub(max_lines);
    let end = start.saturating_add(max_lines).min(search.options.len());
    search
        .options
        .iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|(index, option)| {
            let style = if index == selected {
                THEME.focus_style()
            } else {
                panel_style()
            };
            Line::from(vec![
                Span::raw(if index == selected { "> " } else { "  " }),
                Span::styled(format!("{:<4}", option.kind), style),
                Span::raw(" "),
                Span::styled(truncate(&option.label, 28), style),
                Span::styled("  ", panel_muted_style()),
                Span::styled(truncate(&option.detail, 36), panel_muted_style()),
            ])
        })
        .collect()
}

fn truncate(value: &str, limit: usize) -> String {
    let mut output = String::new();
    for character in value.chars().take(limit) {
        output.push(character);
    }
    if value.chars().count() > limit {
        output.push_str("...");
    }
    output
}

fn composer_input_widget(section: &SessionsSection, area: Rect) -> (Paragraph<'static>, Position) {
    let visible_height = usize::from(area.height.max(1));
    let cursor = section.composer_cursor.min(section.composer_draft.len());
    let (cursor_line, cursor_column) = composer_cursor_line_column(&section.composer_draft, cursor);
    let first_visible_line = cursor_line.saturating_add(1).saturating_sub(visible_height);
    let lines = composer_lines(&section.composer_draft);

    let rendered_lines = if section.composer_draft.is_empty() {
        vec![Line::from(vec![
            Span::raw("  "),
            Span::styled("type a message", panel_muted_style()),
        ])]
    } else {
        lines
            .into_iter()
            .skip(first_visible_line)
            .take(visible_height)
            .map(|line| {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(template_inline_spans(
                    &line,
                    panel_style(),
                    THEME.focus_style(),
                    THEME.status_style(StatusRole::Error),
                ));
                Line::from(spans)
            })
            .collect::<Vec<_>>()
    };

    let visible_cursor_line = cursor_line.saturating_sub(first_visible_line);
    let cursor_x = area
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(cursor_column).unwrap_or(u16::MAX))
        .min(area.right().saturating_sub(1));
    let cursor_y = area
        .y
        .saturating_add(u16::try_from(visible_cursor_line).unwrap_or(u16::MAX))
        .min(area.bottom().saturating_sub(1));

    (
        Paragraph::new(rendered_lines)
            .style(panel_style())
            .wrap(Wrap { trim: false }),
        Position::new(cursor_x, cursor_y),
    )
}

fn render_composer_metadata(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &SessionView,
    context_tokens: u64,
) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)]).areas(area);

    frame.render_widget(
        Paragraph::new(composer_runtime_line(session.runtime_config.as_ref())).style(panel_style()),
        left,
    );
    frame.render_widget(
        Paragraph::new(detail_metrics_line(session, context_tokens))
            .style(panel_style())
            .alignment(Alignment::Right),
        right,
    );
}

fn composer_runtime_line(runtime: Option<&SessionRuntimeConfigView>) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("provider ", panel_muted_style()),
        Span::raw(provider_model_status_text(runtime)),
        Span::raw(" "),
        Span::styled("(C-x p)", THEME.focus_style()),
    ];
    if let Some(thinking) = provider_thinking_status_text(runtime) {
        spans.extend([
            Span::raw("  "),
            Span::styled("t ", panel_muted_style()),
            Span::raw(thinking),
            Span::raw(" "),
            Span::styled("(Ctrl-T)", THEME.focus_style()),
        ]);
    } else {
        spans.extend([
            Span::raw(" "),
            Span::styled("(Ctrl-T)", THEME.focus_style()),
        ]);
    }
    spans.extend([
        Span::raw(" "),
        Span::styled("mcp ", panel_muted_style()),
        Span::raw(mcp_status_text(runtime)),
        Span::raw(" "),
        Span::styled("(C-x m)", THEME.focus_style()),
    ]);
    Line::from(spans)
}

fn detail_metrics_line(session: &SessionView, context_tokens: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled("ctx ", panel_muted_style()),
        Span::styled(format_count(context_tokens), panel_muted_style()),
        Span::styled("  c ", panel_muted_style()),
        Span::styled(
            format!(
                "{}/{}",
                cache_rate(
                    session.metrics.cache_read_tokens,
                    session.metrics.total_input_tokens
                ),
                cache_rate(
                    session.metrics.cache_write_tokens,
                    session.metrics.total_input_tokens
                )
            ),
            panel_muted_style(),
        ),
        Span::styled(
            format!(
                "  {}",
                format_compact_currency_cost(
                    session.metrics.total_cost_micros,
                    session.metrics.cost_currency.as_deref(),
                )
            ),
            panel_muted_style(),
        ),
    ])
}

fn provider_model_status_text(runtime: Option<&SessionRuntimeConfigView>) -> String {
    let Some(provider) = runtime.and_then(|runtime| runtime.provider.as_ref()) else {
        return "none".to_owned();
    };

    format!("{}/{}", provider.key, provider.model_id)
}

fn provider_thinking_status_text(runtime: Option<&SessionRuntimeConfigView>) -> Option<String> {
    let provider = runtime.and_then(|runtime| runtime.provider.as_ref())?;
    Some(
        provider
            .reasoning_effort
            .as_deref()
            .unwrap_or("none")
            .to_owned(),
    )
}

fn mcp_status_text(runtime: Option<&SessionRuntimeConfigView>) -> String {
    let Some(runtime) = runtime else {
        return "none".to_owned();
    };

    if runtime.mcp_servers.is_empty() {
        return "none".to_owned();
    }

    runtime
        .mcp_servers
        .iter()
        .map(|server| server.key.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
