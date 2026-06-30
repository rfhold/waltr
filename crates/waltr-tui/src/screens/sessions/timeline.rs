//! Timeline modeling, rendering, and click hit testing.

use std::collections::{BTreeMap, BTreeSet};

use waltr_client::{
    ClientCommand, ExternalRequestView, SessionTimelineView, TimelineContentView,
    TimelineEventKindView, TimelineEventView, TimelineStatusView,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde_json::Value;

use crate::{
    markdown::{self, MarkdownRenderOptions, MarkdownTone},
    theme::{StatusRole, THEME},
    ui,
};

use super::{
    SHELL_OUTPUT_RECENT_LINE_COUNT, TIMELINE_CHANNELS, TIMELINE_PAGE_SIZE, TIMELINE_TEXT_PREFIX,
    format_compact_cost, format_count, panel_style, prompts::external_request_kind_label,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum TimelineItemModel {
    Text {
        item_id: String,
        label: &'static str,
        text: String,
        status: Option<TimelineStatusView>,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        input_entries: Vec<ToolInputEntry>,
        result_content: String,
        progress_info: String,
        progress_error: String,
        state: ToolState,
    },
    External {
        item_id: String,
        request: Option<ExternalRequestView>,
        resolved: bool,
    },
    Metrics(String),
    Event {
        label: &'static str,
        text: String,
        status: Option<TimelineStatusView>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolInputEntry {
    key: String,
    value: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolState {
    Loading,
    Complete,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ToolFile {
    status: &'static str,
    path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiffLineKind {
    Add,
    Remove,
    File,
    Meta,
    Context,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiffLineModel {
    kind: DiffLineKind,
    text: String,
    language: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiffLineParts {
    prefix: String,
    code: String,
}

pub(super) fn timeline_widget(
    timeline: Option<&SessionTimelineView>,
    scroll: usize,
    width: u16,
    expanded_shell_tools: &BTreeSet<String>,
) -> Paragraph<'static> {
    Paragraph::new(wrap_timeline_lines(
        timeline_lines(timeline, Some(width), expanded_shell_tools),
        width,
    ))
    .style(THEME.text_style())
    .scroll((scroll.min(u16::MAX as usize) as u16, 0))
}

pub(super) fn timeline_rendered_line_count(
    timeline: Option<&SessionTimelineView>,
    width: u16,
    expanded_shell_tools: &BTreeSet<String>,
) -> usize {
    wrap_timeline_lines(
        timeline_lines(timeline, Some(width), expanded_shell_tools),
        width,
    )
    .len()
}

pub(super) fn timeline_lines(
    timeline: Option<&SessionTimelineView>,
    width: Option<u16>,
    expanded_shell_tools: &BTreeSet<String>,
) -> Vec<Line<'static>> {
    let Some(timeline) = timeline else {
        return vec![ui::loading_line("Loading timeline...")];
    };

    if timeline.events.is_empty() {
        let label = if matches!(timeline.watch.status, "opened" | "received") {
            "No timeline events yet."
        } else {
            "Loading timeline..."
        };
        return vec![
            ui::loading_line(label),
            Line::from(vec![
                Span::styled("watch ", THEME.muted_text_style()),
                Span::styled(timeline.watch.status.to_owned(), THEME.focus_style()),
            ]),
        ];
    }

    let mut lines = Vec::new();
    for item in timeline_items(&timeline.events) {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        append_timeline_item_lines(&mut lines, item, width, expanded_shell_tools);
    }
    lines
}

pub(super) fn shell_tool_at_row(
    timeline: &SessionTimelineView,
    width: u16,
    row: usize,
    expanded_shell_tools: &BTreeSet<String>,
) -> Option<String> {
    let mut offset = 0usize;
    for item in timeline_items(&timeline.events) {
        if offset > 0 {
            if row == offset {
                return None;
            }
            offset = offset.saturating_add(1);
        }

        let shell_id = match &item {
            TimelineItemModel::Tool { id, name, .. } if name == "shell" => Some(id.clone()),
            _ => None,
        };
        let mut item_lines = Vec::new();
        append_timeline_item_lines(&mut item_lines, item, Some(width), expanded_shell_tools);
        let rendered_height = wrap_timeline_lines(item_lines, width).len();
        if row >= offset && row < offset.saturating_add(rendered_height) {
            return shell_id;
        }
        offset = offset.saturating_add(rendered_height);
    }
    None
}

pub(super) fn latest_context_tokens(timeline: Option<&SessionTimelineView>) -> u64 {
    timeline
        .and_then(|timeline| {
            timeline
                .events
                .iter()
                .rev()
                .find_map(context_tokens_from_metrics_event)
        })
        .unwrap_or_default()
}

fn context_tokens_from_metrics_event(event: &TimelineEventView) -> Option<u64> {
    if event.channel.as_deref() != Some("metrics") {
        return None;
    }

    let TimelineContentView::Json { value_json } = &event.content else {
        return None;
    };
    let value = serde_json::from_str::<Value>(value_json).ok()?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !kind.is_empty() && kind != "turn_metrics" {
        return None;
    }

    if let Some(context) = json_u64_path(&value, &["usage", "context_tokens"])
        .or_else(|| json_u64_path(&value, &["usage", "contextTokens"]))
    {
        return Some(context);
    }

    let input = usage_total_input_tokens(&value);
    let output = json_u64_path(&value, &["usage", "output_tokens"])
        .or_else(|| json_u64_path(&value, &["usage", "outputTokens"]))
        .unwrap_or_default();

    Some(input + output)
}

fn timeline_items(events: &[TimelineEventView]) -> Vec<TimelineItemModel> {
    let mut items = Vec::new();
    let mut tool_id_by_item_id = BTreeMap::new();

    for event in events {
        match event.channel.as_deref() {
            Some("user_text" | "assistant_text" | "thinking") => {
                upsert_text_item(&mut items, event);
            }
            Some("tool_call") => {
                upsert_tool_call_item(&mut items, &mut tool_id_by_item_id, event);
            }
            Some("tool_result") => {
                upsert_tool_result_item(&mut items, &tool_id_by_item_id, event);
            }
            Some("tool_progress_info") => {
                append_tool_progress(&mut items, &tool_id_by_item_id, event, false);
            }
            Some("tool_progress_error") => {
                append_tool_progress(&mut items, &tool_id_by_item_id, event, true);
            }
            Some("external_request") => {
                upsert_external_item(&mut items, event);
            }
            Some("metrics") => {
                if let Some(metrics) = metrics_line(event) {
                    items.push(TimelineItemModel::Metrics(metrics));
                }
            }
            _ => items.push(TimelineItemModel::Event {
                label: event_label(event),
                text: event_text(event),
                status: event.status,
            }),
        }
    }

    items
}

fn event_label(event: &TimelineEventView) -> &'static str {
    match event.channel.as_deref() {
        Some("user_text") => "user",
        Some("assistant_text") => "assistant",
        Some("thinking") => "thinking",
        Some("tool_call") => "tool call",
        Some("tool_progress_info") => "tool info",
        Some("tool_progress_error") => "tool error",
        Some("tool_result") => "tool result",
        Some("external_request") => "external",
        Some("metrics") => "metrics",
        _ => match event.kind {
            TimelineEventKindView::ExternalRequestOpened => "external opened",
            TimelineEventKindView::ExternalRequestResolved => "external resolved",
            TimelineEventKindView::SessionStatusChanged => "status",
            TimelineEventKindView::ContentDelta => "delta",
            TimelineEventKindView::ItemStarted => "started",
            TimelineEventKindView::ItemUpdated => "updated",
            TimelineEventKindView::ItemFinished => "finished",
            TimelineEventKindView::Unspecified | TimelineEventKindView::Unknown => "event",
        },
    }
}

fn upsert_text_item(items: &mut Vec<TimelineItemModel>, event: &TimelineEventView) {
    let label = text_channel_label(event.channel.as_deref());
    let content = match &event.content {
        TimelineContentView::TextDelta { delta } => delta.clone(),
        TimelineContentView::TextSnapshot { text } => text.clone(),
        _ => event_text(event),
    };

    if let Some(TimelineItemModel::Text { text, status, .. }) = items.iter_mut().find(|item| {
        matches!(
            item,
            TimelineItemModel::Text { item_id, .. } if item_id == &event.item_id
        )
    }) {
        match &event.content {
            TimelineContentView::TextSnapshot { .. } => *text = content,
            _ => text.push_str(&content),
        }
        *status = event.status;
        return;
    }

    items.push(TimelineItemModel::Text {
        item_id: event.item_id.clone(),
        label,
        text: content,
        status: event.status,
    });
}

fn upsert_tool_call_item(
    items: &mut Vec<TimelineItemModel>,
    tool_id_by_item_id: &mut BTreeMap<String, String>,
    event: &TimelineEventView,
) {
    let TimelineContentView::ToolCall {
        id,
        name,
        arguments_json,
    } = &event.content
    else {
        items.push(TimelineItemModel::Event {
            label: event_label(event),
            text: event_text(event),
            status: event.status,
        });
        return;
    };
    let id = non_empty_id(id, &event.item_id);
    tool_id_by_item_id.insert(event.item_id.clone(), id.clone());
    let input = parse_json_record(arguments_json);
    let input_entries = top_level_input_entries(&input);

    if let Some(TimelineItemModel::Tool {
        name: item_name,
        input: item_input,
        input_entries: item_input_entries,
        state,
        ..
    }) = find_tool_item_mut(items, &id)
    {
        if !name.is_empty() {
            *item_name = name.clone();
        }
        *item_input = input;
        *item_input_entries = input_entries;
        *state = ToolState::Loading;
        return;
    }

    items.push(TimelineItemModel::Tool {
        id,
        name: name.clone(),
        input,
        input_entries,
        result_content: String::new(),
        progress_info: String::new(),
        progress_error: String::new(),
        state: ToolState::Loading,
    });
}

fn upsert_tool_result_item(
    items: &mut Vec<TimelineItemModel>,
    tool_id_by_item_id: &BTreeMap<String, String>,
    event: &TimelineEventView,
) {
    let TimelineContentView::ToolResult {
        id,
        name,
        content,
        error,
    } = &event.content
    else {
        items.push(TimelineItemModel::Event {
            label: event_label(event),
            text: event_text(event),
            status: event.status,
        });
        return;
    };
    let id = if id.is_empty() {
        event
            .parent_id
            .as_ref()
            .and_then(|parent_id| tool_id_by_item_id.get(parent_id))
            .cloned()
            .unwrap_or_else(|| event.item_id.clone())
    } else {
        id.clone()
    };

    if let Some(TimelineItemModel::Tool {
        name: item_name,
        result_content,
        state,
        ..
    }) = find_tool_item_mut(items, &id)
    {
        if !name.is_empty() {
            *item_name = name.clone();
        }
        *result_content = content.clone();
        *state = if *error {
            ToolState::Error
        } else {
            ToolState::Complete
        };
        return;
    }

    items.push(TimelineItemModel::Tool {
        id,
        name: name.clone(),
        input: empty_json_object(),
        input_entries: Vec::new(),
        result_content: content.clone(),
        progress_info: String::new(),
        progress_error: String::new(),
        state: if *error {
            ToolState::Error
        } else {
            ToolState::Complete
        },
    });
}

fn append_tool_progress(
    items: &mut Vec<TimelineItemModel>,
    tool_id_by_item_id: &BTreeMap<String, String>,
    event: &TimelineEventView,
    error: bool,
) {
    let Some((progress, snapshot)) = tool_progress_content(event) else {
        return;
    };
    let tool_item_id = event.parent_id.as_deref().unwrap_or(event.item_id.as_str());
    let id = tool_id_by_item_id
        .get(tool_item_id)
        .map_or(tool_item_id, String::as_str);
    if let Some(TimelineItemModel::Tool {
        progress_info,
        progress_error,
        ..
    }) = find_tool_item_mut(items, id)
    {
        let target = if error { progress_error } else { progress_info };
        if error {
            if snapshot {
                *target = progress;
            } else {
                target.push_str(&progress);
            }
        } else if snapshot {
            *target = progress;
        } else {
            target.push_str(&progress);
        }
    }
}

fn upsert_external_item(items: &mut Vec<TimelineItemModel>, event: &TimelineEventView) {
    let request = match &event.content {
        TimelineContentView::ExternalRequest { request } => request.clone(),
        _ => None,
    };
    let resolved = event.kind == TimelineEventKindView::ExternalRequestResolved
        || event.status == Some(TimelineStatusView::Resolved);

    if let Some(TimelineItemModel::External {
        request: item_request,
        resolved: item_resolved,
        ..
    }) = items.iter_mut().find(|item| {
        matches!(
            item,
            TimelineItemModel::External { item_id, .. } if item_id == &event.item_id
        )
    }) {
        if request.is_some() {
            *item_request = request;
        }
        *item_resolved = *item_resolved || resolved;
        return;
    }

    items.push(TimelineItemModel::External {
        item_id: event.item_id.clone(),
        request,
        resolved,
    });
}

fn find_tool_item_mut<'a>(
    items: &'a mut [TimelineItemModel],
    id: &str,
) -> Option<&'a mut TimelineItemModel> {
    items.iter_mut().find(|item| {
        matches!(
            item,
            TimelineItemModel::Tool { id: item_id, .. } if item_id == id
        )
    })
}

fn append_timeline_item_lines(
    lines: &mut Vec<Line<'static>>,
    item: TimelineItemModel,
    width: Option<u16>,
    expanded_shell_tools: &BTreeSet<String>,
) {
    match item {
        TimelineItemModel::Text {
            label,
            text,
            status,
            ..
        } => append_text_item_lines(lines, label, &text, status, width),
        TimelineItemModel::Tool {
            id,
            name,
            input,
            input_entries,
            result_content,
            progress_info,
            progress_error,
            state,
        } => append_tool_lines(
            lines,
            &id,
            &name,
            &input,
            &input_entries,
            &result_content,
            &progress_info,
            &progress_error,
            state,
            width,
            expanded_shell_tools,
        ),
        TimelineItemModel::External {
            request, resolved, ..
        } => {
            let label = request.as_ref().map_or("external request", |request| {
                external_request_kind_label(request.kind)
            });
            let status = if resolved { "answered" } else { "pending" };
            lines.push(Line::from(vec![
                Span::styled("~ ", THEME.focus_style()),
                Span::styled(label.to_owned(), THEME.text_style()),
                Span::raw("  "),
                Span::styled(status, THEME.muted_text_style()),
            ]));
        }
        TimelineItemModel::Metrics(metrics) => lines.push(metrics_item_line(&metrics, width)),
        TimelineItemModel::Event {
            label,
            text,
            status,
        } => {
            lines.push(label_line(label, status));
            if !text.is_empty() {
                append_indented_text_lines(lines, &text, THEME.text_style());
            }
        }
    }
}

fn append_text_item_lines(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    text: &str,
    status: Option<TimelineStatusView>,
    width: Option<u16>,
) {
    match label {
        "user" => append_markdown_text_lines(lines, text, MarkdownTone::User, width),
        "assistant" => append_markdown_text_lines(lines, text, MarkdownTone::Assistant, width),
        "thinking" => append_markdown_text_lines(lines, text, MarkdownTone::Thinking, width),
        _ => {
            lines.push(label_line(label, status));
            append_indented_text_lines(lines, text, THEME.text_style());
        }
    }
}

fn append_markdown_text_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    tone: MarkdownTone,
    width: Option<u16>,
) {
    let options = match tone {
        MarkdownTone::User => MarkdownRenderOptions {
            tone,
            first_prefix: "",
            rest_prefix: "",
            prefix_style: panel_style(),
            base_style: panel_style(),
        },
        MarkdownTone::Assistant => MarkdownRenderOptions {
            tone,
            first_prefix: TIMELINE_TEXT_PREFIX,
            rest_prefix: TIMELINE_TEXT_PREFIX,
            prefix_style: THEME.text_style(),
            base_style: THEME.text_style(),
        },
        MarkdownTone::Thinking => MarkdownRenderOptions {
            tone,
            first_prefix: TIMELINE_TEXT_PREFIX,
            rest_prefix: TIMELINE_TEXT_PREFIX,
            prefix_style: THEME.muted_text_style(),
            base_style: THEME.muted_text_style(),
        },
    };
    let rendered = markdown::render_markdown(text, options);
    if tone == MarkdownTone::User {
        append_user_message_lines(lines, rendered, width);
    } else {
        lines.extend(rendered);
    }
}

fn append_user_message_lines(
    lines: &mut Vec<Line<'static>>,
    rendered: Vec<Line<'static>>,
    width: Option<u16>,
) {
    lines.push(user_message_line(Vec::new(), width));
    for line in rendered {
        let mut spans = vec![accent_border_span(), Span::styled("  ", panel_style())];
        spans.extend(line.spans.into_iter().map(span_with_panel_bg));
        lines.push(user_message_line_with_prefix(spans, width));
    }
    lines.push(user_message_line(Vec::new(), width));
}

fn user_message_line(mut spans: Vec<Span<'static>>, width: Option<u16>) -> Line<'static> {
    let mut line_spans = vec![accent_border_span(), Span::styled("  ", panel_style())];
    line_spans.append(&mut spans);
    user_message_line_with_prefix(line_spans, width)
}

fn user_message_line_with_prefix(
    mut spans: Vec<Span<'static>>,
    width: Option<u16>,
) -> Line<'static> {
    if let Some(width) = width {
        let used = spans_width(&spans);
        let target = usize::from(width);
        if used < target {
            spans.push(Span::styled(" ".repeat(target - used), panel_style()));
        }
    }
    Line::from(spans).style(panel_style())
}

pub(super) fn accent_border_span() -> Span<'static> {
    Span::styled("▎", Style::default().fg(THEME.accent()))
}

fn span_with_panel_bg(mut span: Span<'static>) -> Span<'static> {
    span.style = span.style.bg(THEME.panel());
    span
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn wrap_timeline_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return lines;
    }

    let mut wrapped = Vec::new();
    for line in lines {
        if spans_width(&line.spans) <= width {
            wrapped.push(line);
            continue;
        }

        let style = line.style;
        let (prefix, mut rest) = split_line_prefix(line.spans);
        let prefix_width = spans_width(&prefix).min(width.saturating_sub(1));
        let content_width = width.saturating_sub(prefix_width).max(1);

        while !rest.is_empty() {
            let (chunk, remaining) = take_spans_width(rest, content_width);
            let mut spans = prefix.clone();
            spans.extend(chunk);
            wrapped.push(Line::from(spans).style(style));
            rest = remaining;
        }
    }

    wrapped
}

fn split_line_prefix(spans: Vec<Span<'static>>) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let mut prefix = Vec::new();
    let mut rest = Vec::new();
    let mut in_prefix = true;

    for span in spans {
        if !in_prefix {
            rest.push(span);
            continue;
        }

        let content = span.content.to_string();
        if prefix.is_empty() && content == "▎" {
            prefix.push(span);
            continue;
        }

        let prefix_len = content.chars().take_while(|ch| ch.is_whitespace()).count();
        if prefix_len == content.chars().count() {
            prefix.push(span);
            continue;
        }

        in_prefix = false;
        if prefix_len == 0 {
            rest.push(span);
        } else {
            let (prefix_span, rest_span) = split_span_at_chars(span, prefix_len);
            if let Some(prefix_span) = prefix_span {
                prefix.push(prefix_span);
            }
            if let Some(rest_span) = rest_span {
                rest.push(rest_span);
            }
        }
    }

    (prefix, rest)
}

fn take_spans_width(
    spans: Vec<Span<'static>>,
    width: usize,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let mut taken = Vec::new();
    let mut remaining = Vec::new();
    let mut used = 0;
    let mut taking = true;

    for span in spans {
        if !taking {
            remaining.push(span);
            continue;
        }

        let span_width = span.content.chars().count();
        if used + span_width <= width {
            used += span_width;
            taken.push(span);
            continue;
        }

        let available = width.saturating_sub(used);
        let (left, right) = split_span_at_chars(span, available);
        if let Some(left) = left {
            taken.push(left);
        }
        if let Some(right) = right {
            remaining.push(right);
        }
        taking = false;
    }

    (taken, remaining)
}

fn split_span_at_chars(
    span: Span<'static>,
    chars: usize,
) -> (Option<Span<'static>>, Option<Span<'static>>) {
    let content = span.content.to_string();
    let left = content.chars().take(chars).collect::<String>();
    let right = content.chars().skip(chars).collect::<String>();
    let style = span.style;
    let left = (!left.is_empty()).then(|| Span::styled(left, style));
    let right = (!right.is_empty()).then(|| Span::styled(right, style));
    (left, right)
}

fn append_tool_lines(
    lines: &mut Vec<Line<'static>>,
    id: &str,
    name: &str,
    input: &Value,
    input_entries: &[ToolInputEntry],
    result_content: &str,
    progress_info: &str,
    progress_error: &str,
    state: ToolState,
    width: Option<u16>,
    expanded_shell_tools: &BTreeSet<String>,
) {
    match name {
        "shell" => append_shell_tool_lines(
            lines,
            input,
            result_content,
            progress_info,
            progress_error,
            state,
            width,
            expanded_shell_tools.contains(id),
        ),
        "edit" => append_edit_tool_lines(lines, input, state, width),
        "apply_patch" => append_patch_tool_lines(lines, input, result_content, state, width),
        _ => append_generic_tool_lines(lines, name, input_entries, state),
    }
}

fn append_generic_tool_lines(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    input_entries: &[ToolInputEntry],
    state: ToolState,
) {
    let mut spans = vec![
        Span::styled(TIMELINE_TEXT_PREFIX, THEME.muted_text_style()),
        Span::styled("* ", THEME.muted_text_style()),
        Span::styled(
            tool_label(name, input_entries),
            THEME.muted_text_style().add_modifier(Modifier::BOLD),
        ),
    ];
    append_tool_state_spans(&mut spans, state);
    lines.push(Line::from(spans));
}

fn append_shell_tool_lines(
    lines: &mut Vec<Line<'static>>,
    input: &Value,
    result_content: &str,
    progress_info: &str,
    progress_error: &str,
    state: ToolState,
    width: Option<u16>,
    expanded: bool,
) {
    let mut tool_lines = Vec::new();
    let command = string_input(input, "command")
        .filter(|command| !command.is_empty())
        .unwrap_or_else(|| "(empty command)".to_owned());
    let description = string_input(input, "description")
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| "Shell command".to_owned());
    let hint = if expanded { "collapse" } else { "expand" };
    let mut header = vec![
        Span::styled(description, THEME.muted_text_style()),
        Span::styled(format!("  click {hint}"), THEME.muted_text_style()),
    ];
    append_tool_state_spans(&mut header, state);
    tool_lines.push(Line::from(header));
    tool_lines.push(Line::default());
    let mut command_spans = vec![Span::styled("$ ", THEME.muted_text_style())];
    command_spans.extend(markdown::highlighted_code_spans(
        &command,
        Some("bash"),
        THEME.text_style(),
    ));
    tool_lines.push(Line::from(command_spans));

    let output = if progress_info.is_empty() && progress_error.is_empty() {
        result_content.to_owned()
    } else {
        format!("{progress_info}{progress_error}")
    };
    if !output.is_empty() {
        append_shell_output_lines(&mut tool_lines, &output, state, expanded);
    }

    append_tool_panel_lines(lines, tool_lines, width);
}

fn append_shell_output_lines(
    lines: &mut Vec<Line<'static>>,
    output: &str,
    state: ToolState,
    expanded: bool,
) {
    let output_lines = shell_output_lines(output);
    let output_style = if state == ToolState::Error {
        THEME.danger_style()
    } else {
        THEME.text_style()
    };
    if !expanded && output_lines.len() > SHELL_OUTPUT_RECENT_LINE_COUNT {
        lines.push(Line::from(vec![
            Span::styled("... ", THEME.muted_text_style()),
            Span::raw(format!(
                "last {} of {} lines",
                SHELL_OUTPUT_RECENT_LINE_COUNT,
                output_lines.len()
            )),
        ]));
        for line in output_lines
            .iter()
            .skip(output_lines.len() - SHELL_OUTPUT_RECENT_LINE_COUNT)
        {
            lines.push(Line::from(Span::styled((*line).to_owned(), output_style)));
        }
        return;
    }

    for line in output_lines {
        lines.push(Line::from(Span::styled(line.to_owned(), output_style)));
    }
}

fn append_edit_tool_lines(
    lines: &mut Vec<Line<'static>>,
    input: &Value,
    state: ToolState,
    width: Option<u16>,
) {
    let mut tool_lines = Vec::new();
    let file_path = string_input(input, "filePath")
        .filter(|file_path| !file_path.is_empty())
        .unwrap_or_else(|| "Unknown file".to_owned());
    let old_string = string_input(input, "oldString").unwrap_or_default();
    let new_string = string_input(input, "newString").unwrap_or_default();

    append_file_tool_header(&mut tool_lines, "edit", state);
    append_tool_files(
        &mut tool_lines,
        &[ToolFile {
            status: "M",
            path: file_path.clone(),
        }],
    );
    append_diff_line(
        &mut tool_lines,
        &DiffLineModel {
            kind: DiffLineKind::File,
            text: format!("--- {file_path}"),
            language: None,
        },
        None,
    );
    append_diff_line(
        &mut tool_lines,
        &DiffLineModel {
            kind: DiffLineKind::File,
            text: format!("+++ {file_path}"),
            language: None,
        },
        None,
    );
    let language = markdown::language_from_file_path(&file_path);
    for diff_line in diff_text_lines(&old_string, DiffLineKind::Remove) {
        append_diff_line(&mut tool_lines, &diff_line, language);
    }
    for diff_line in diff_text_lines(&new_string, DiffLineKind::Add) {
        append_diff_line(&mut tool_lines, &diff_line, language);
    }
    append_tool_panel_lines(lines, tool_lines, width);
}

fn append_patch_tool_lines(
    lines: &mut Vec<Line<'static>>,
    input: &Value,
    result_content: &str,
    state: ToolState,
    width: Option<u16>,
) {
    let mut tool_lines = Vec::new();
    let patch_text = string_input(input, "patchText").unwrap_or_default();

    append_file_tool_header(&mut tool_lines, "apply_patch", state);
    append_tool_files(&mut tool_lines, &patch_files(&patch_text));
    for diff_line in patch_diff_lines(&patch_text) {
        append_diff_line(&mut tool_lines, &diff_line, diff_line.language.as_deref());
    }
    if !result_content.is_empty() {
        append_indented_text_lines(&mut tool_lines, result_content, THEME.muted_text_style());
    }
    append_tool_panel_lines(lines, tool_lines, width);
}

fn append_tool_panel_lines(
    lines: &mut Vec<Line<'static>>,
    tool_lines: Vec<Line<'static>>,
    width: Option<u16>,
) {
    lines.push(tool_panel_line(Vec::new(), width));
    for line in tool_lines {
        let mut spans = vec![Span::styled(TIMELINE_TEXT_PREFIX, panel_style())];
        spans.extend(line.spans.into_iter().map(span_with_panel_bg));
        lines.push(tool_panel_line_with_prefix(spans, width));
    }
    lines.push(tool_panel_line(Vec::new(), width));
}

fn tool_panel_line(mut spans: Vec<Span<'static>>, width: Option<u16>) -> Line<'static> {
    let mut line_spans = vec![Span::styled(TIMELINE_TEXT_PREFIX, panel_style())];
    line_spans.append(&mut spans);
    tool_panel_line_with_prefix(line_spans, width)
}

fn tool_panel_line_with_prefix(mut spans: Vec<Span<'static>>, width: Option<u16>) -> Line<'static> {
    if let Some(width) = width {
        let used = spans_width(&spans);
        let target = usize::from(width);
        if used < target {
            spans.push(Span::styled(" ".repeat(target - used), panel_style()));
        }
    }
    Line::from(spans).style(panel_style())
}

fn append_file_tool_header(lines: &mut Vec<Line<'static>>, name: &str, state: ToolState) {
    let mut spans = vec![
        Span::styled("* ", THEME.muted_text_style()),
        Span::styled(name.to_owned(), tool_header_style(state)),
    ];
    append_tool_state_spans(&mut spans, state);
    lines.push(Line::from(spans));
}

fn append_tool_files(lines: &mut Vec<Line<'static>>, files: &[ToolFile]) {
    if files.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ? ", THEME.muted_text_style()),
            Span::raw("Unknown files"),
        ]));
        return;
    }

    for file in files {
        lines.push(Line::from(vec![
            Span::styled("  -> ", THEME.muted_text_style()),
            Span::styled(format!("{} ", file.status), THEME.focus_style()),
            Span::raw(file.path.clone()),
        ]));
    }
}

fn append_diff_line(
    lines: &mut Vec<Line<'static>>,
    diff_line: &DiffLineModel,
    language: Option<&str>,
) {
    let style = diff_line_style(diff_line.kind);
    let parts = diff_line_parts(&diff_line.text, diff_line.kind);
    let mut spans = vec![
        Span::styled("  ".to_owned(), style),
        Span::styled(parts.prefix, style),
    ];
    if matches!(
        diff_line.kind,
        DiffLineKind::Add | DiffLineKind::Remove | DiffLineKind::Context
    ) {
        spans.extend(markdown::highlighted_code_spans(
            &parts.code,
            language,
            style,
        ));
    } else {
        spans.push(Span::styled(parts.code, style));
    }
    lines.push(Line::from(spans));
}

fn diff_line_parts(text: &str, kind: DiffLineKind) -> DiffLineParts {
    if matches!(kind, DiffLineKind::Add | DiffLineKind::Remove) {
        return DiffLineParts {
            prefix: text.get(..1).unwrap_or_default().to_owned(),
            code: text.get(1..).unwrap_or_default().to_owned(),
        };
    }
    if kind == DiffLineKind::Context && text.starts_with(' ') {
        return DiffLineParts {
            prefix: " ".to_owned(),
            code: text.get(1..).unwrap_or_default().to_owned(),
        };
    }
    DiffLineParts {
        prefix: String::new(),
        code: text.to_owned(),
    }
}

fn append_tool_state_spans(spans: &mut Vec<Span<'static>>, state: ToolState) {
    match state {
        ToolState::Loading => {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                "loading...",
                THEME.status_style(StatusRole::Loading),
            ));
        }
        ToolState::Error => {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("error", THEME.danger_style()));
        }
        ToolState::Complete => {}
    }
}

fn tool_header_style(state: ToolState) -> ratatui::style::Style {
    if state == ToolState::Error {
        THEME.danger_style().add_modifier(Modifier::BOLD)
    } else {
        THEME.focus_style().add_modifier(Modifier::BOLD)
    }
}

fn diff_line_style(kind: DiffLineKind) -> ratatui::style::Style {
    match kind {
        DiffLineKind::Add => THEME.success_style(),
        DiffLineKind::Remove => THEME.danger_style(),
        DiffLineKind::File | DiffLineKind::Meta => THEME.dim_accent_style(),
        DiffLineKind::Context => THEME.muted_text_style(),
    }
}

fn append_indented_text_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    style: ratatui::style::Style,
) {
    if text.is_empty() {
        lines.push(Line::from(Span::styled(TIMELINE_TEXT_PREFIX, style)));
        return;
    }

    for line in text.lines() {
        lines.push(Line::from(Span::styled(
            format!("{TIMELINE_TEXT_PREFIX}{line}"),
            style,
        )));
    }
}

fn label_line(label: &str, status: Option<TimelineStatusView>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(TIMELINE_TEXT_PREFIX, THEME.focus_style()),
        Span::styled(label.to_owned(), THEME.focus_style()),
    ];
    if let Some(status) = status {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(status_name(status), THEME.muted_text_style()));
    }
    Line::from(spans)
}

fn text_channel_label(channel: Option<&str>) -> &'static str {
    match channel {
        Some("user_text") => "user",
        Some("assistant_text") => "assistant",
        Some("thinking") => "thinking",
        _ => "text",
    }
}

fn metrics_line(event: &TimelineEventView) -> Option<String> {
    let TimelineContentView::Json { value_json } = &event.content else {
        return Some(event_text(event));
    };
    let value = serde_json::from_str::<Value>(value_json).ok()?;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !kind.is_empty() && kind != "turn_metrics" {
        return None;
    }

    let input = usage_total_input_tokens(&value);
    let output = json_u64_path(&value, &["usage", "output_tokens"])
        .or_else(|| json_u64_path(&value, &["usage", "outputTokens"]));
    let cost = json_u64_path(&value, &["cost", "total_micros"])
        .or_else(|| json_u64_path(&value, &["cost", "totalMicros"]));

    let mut parts = Vec::new();
    if input > 0 {
        parts.push(format!("i{}", format_count(input)));
    }
    if let Some(output) = output {
        parts.push(format!("o{}", format_count(output)));
    }
    if let Some(cost) = cost {
        parts.push(format_compact_cost(cost));
    }

    if parts.is_empty() {
        Some(value_json.clone())
    } else {
        Some(parts.join("  "))
    }
}

fn usage_total_input_tokens(value: &Value) -> u64 {
    if let Some(total) = json_u64_path(value, &["usage", "total_input_tokens"])
        .or_else(|| json_u64_path(value, &["usage", "totalInputTokens"]))
    {
        return total;
    }

    json_u64_path(value, &["usage", "input_tokens"])
        .or_else(|| json_u64_path(value, &["usage", "inputTokens"]))
        .unwrap_or_default()
        + json_u64_path(value, &["usage", "cache_read_tokens"])
            .or_else(|| json_u64_path(value, &["usage", "cacheReadTokens"]))
            .unwrap_or_default()
        + json_u64_path(value, &["usage", "cache_write_tokens"])
            .or_else(|| json_u64_path(value, &["usage", "cacheWriteTokens"]))
            .unwrap_or_default()
        + json_u64_path(value, &["usage", "image_input_tokens"])
            .or_else(|| json_u64_path(value, &["usage", "imageInputTokens"]))
            .unwrap_or_default()
}

fn metrics_item_line(metrics: &str, width: Option<u16>) -> Line<'static> {
    let text = format!("· {metrics}");
    let text_width = text.chars().count();
    let leading = width
        .map(|width| usize::from(width).saturating_sub(text_width))
        .unwrap_or(TIMELINE_TEXT_PREFIX.len());
    Line::from(vec![Span::styled(
        format!("{}{text}", " ".repeat(leading)),
        THEME.muted_text_style(),
    )])
}

fn event_text(event: &TimelineEventView) -> String {
    match &event.content {
        TimelineContentView::Empty => String::new(),
        TimelineContentView::TextDelta { delta } => delta.clone(),
        TimelineContentView::TextSnapshot { text } => text.clone(),
        TimelineContentView::ToolCall {
            id,
            name,
            arguments_json,
        } => format!("{name} {id} {arguments_json}"),
        TimelineContentView::ToolResult {
            id,
            name,
            content,
            error,
        } => {
            let prefix = if *error { "error" } else { "ok" };
            format!("{name} {id} {prefix}: {content}")
        }
        TimelineContentView::ExternalRequest { request } => request.as_ref().map_or_else(
            || "request pending".to_owned(),
            |request| format!("request {} kind {}", request.id, request.kind),
        ),
        TimelineContentView::ContentRef { content_id, bytes } => {
            format!("content {content_id} ({bytes} bytes)")
        }
        TimelineContentView::Json { value_json } => value_json.clone(),
        TimelineContentView::Unknown => "unknown content".to_owned(),
    }
}

const fn status_name(status: TimelineStatusView) -> &'static str {
    match status {
        TimelineStatusView::Unspecified => "unspecified",
        TimelineStatusView::Idle => "idle",
        TimelineStatusView::BuildingRequest => "building request",
        TimelineStatusView::StreamingProvider => "streaming provider",
        TimelineStatusView::EvaluatingToolCall => "evaluating tool call",
        TimelineStatusView::RunningTool => "running tool",
        TimelineStatusView::Pending => "pending",
        TimelineStatusView::AwaitingToolResults => "awaiting tool results",
        TimelineStatusView::AwaitingExternalInput => "awaiting external input",
        TimelineStatusView::Interrupted => "interrupted",
        TimelineStatusView::Resolved => "resolved",
        TimelineStatusView::Finished => "finished",
        TimelineStatusView::Failed => "failed",
        TimelineStatusView::Canceled => "canceled",
        TimelineStatusView::Unknown => "unknown",
    }
}

fn json_u64_path(value: &Value, path: &[&str]) -> Option<u64> {
    path.iter()
        .try_fold(value, |value, key| value.get(*key))
        .and_then(Value::as_u64)
}

fn non_empty_id(id: &str, fallback: &str) -> String {
    if id.is_empty() {
        fallback.to_owned()
    } else {
        id.to_owned()
    }
}

fn empty_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn parse_json_record(value_json: &str) -> Value {
    serde_json::from_str::<Value>(value_json)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(empty_json_object)
}

fn top_level_input_entries(input: &Value) -> Vec<ToolInputEntry> {
    let Some(object) = input.as_object() else {
        return Vec::new();
    };
    let mut entries = object
        .iter()
        .map(|(key, value)| ToolInputEntry {
            key: key.clone(),
            value: format_tool_input_value(value),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

fn format_tool_input_value(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn tool_label(name: &str, input_entries: &[ToolInputEntry]) -> String {
    if input_entries.is_empty() {
        return name.to_owned();
    }

    let entries = input_entries
        .iter()
        .map(|entry| format!("{}: {}", entry.key, entry.value))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({entries})")
}

fn string_input(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn tool_progress_content(event: &TimelineEventView) -> Option<(String, bool)> {
    match &event.content {
        TimelineContentView::TextDelta { delta } => Some((delta.clone(), false)),
        TimelineContentView::TextSnapshot { text } => Some((text.clone(), true)),
        _ => None,
    }
}

fn shell_output_lines(output: &str) -> Vec<&str> {
    let mut lines = output.split('\n').collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn diff_text_lines(text: &str, kind: DiffLineKind) -> Vec<DiffLineModel> {
    let prefix = if kind == DiffLineKind::Add { '+' } else { '-' };
    if text.is_empty() {
        return vec![DiffLineModel {
            kind,
            text: format!("{prefix} "),
            language: None,
        }];
    }

    text.split('\n')
        .map(|line| DiffLineModel {
            kind,
            text: format!("{prefix} {line}"),
            language: None,
        })
        .collect()
}

fn patch_files(patch_text: &str) -> Vec<ToolFile> {
    patch_text
        .split('\n')
        .filter_map(|line| {
            line.strip_prefix("*** Add File: ")
                .map(|path| ToolFile {
                    status: "A",
                    path: path.to_owned(),
                })
                .or_else(|| {
                    line.strip_prefix("*** Update File: ").map(|path| ToolFile {
                        status: "M",
                        path: path.to_owned(),
                    })
                })
                .or_else(|| {
                    line.strip_prefix("*** Delete File: ").map(|path| ToolFile {
                        status: "D",
                        path: path.to_owned(),
                    })
                })
        })
        .collect()
}

fn patch_diff_lines(patch_text: &str) -> Vec<DiffLineModel> {
    let mut language = None;
    patch_text
        .split('\n')
        .map(|line| {
            if let Some(path) = line
                .strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
            {
                language = markdown::language_from_file_path(path).map(str::to_owned);
            }
            DiffLineModel {
                kind: patch_line_kind(line),
                text: line.to_owned(),
                language: language.clone(),
            }
        })
        .collect()
}

fn patch_line_kind(line: &str) -> DiffLineKind {
    if line.starts_with("***") {
        return DiffLineKind::File;
    }
    if line.starts_with("@@") {
        return DiffLineKind::Meta;
    }
    if line.starts_with('+') {
        return DiffLineKind::Add;
    }
    if line.starts_with('-') {
        return DiffLineKind::Remove;
    }
    DiffLineKind::Context
}

pub(super) fn timeline_channels() -> Vec<String> {
    TIMELINE_CHANNELS
        .iter()
        .map(|channel| (*channel).to_owned())
        .collect()
}

pub(crate) fn register_timeline_watch_command(session_id: String) -> ClientCommand {
    ClientCommand::RegisterSessionTimelineWatchInterest {
        session_id,
        after_cursor: None,
        channels: timeline_channels(),
        initial_limit: Some(TIMELINE_PAGE_SIZE),
    }
}
