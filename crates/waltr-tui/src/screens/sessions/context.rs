//! Read-only session associations and aggregate context rendering.

use waltr_client::{
    ClientRoutineInvocationStatus, ClientRoutineInvocationSummary, ClientSnapshot,
    SessionProgressPhaseView, SessionRunStageView, SessionView,
};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

use crate::theme::{StatusRole, THEME};

use super::{
    cache_rate, format_compact_currency_cost, format_count, panel_muted_style, panel_style,
    progress_coordination_status,
};

pub(super) fn render_session_context(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&ClientSnapshot>,
    selected: &SessionView,
    context_tokens: u64,
    summarize_overflow: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let content_area = if summarize_overflow {
        area
    } else {
        area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        })
    };
    frame.render_widget(Block::default().style(panel_style()), area);
    if content_area.width == 0 || content_area.height == 0 {
        return;
    }

    let mut lines = if summarize_overflow {
        compact_context_lines(snapshot, selected, context_tokens)
    } else {
        context_lines(snapshot, selected, context_tokens)
    };
    if summarize_overflow && lines.len() > usize::from(content_area.height) {
        let visible = usize::from(content_area.height);
        let hidden = lines.len().saturating_sub(visible).saturating_add(1);
        lines.truncate(visible.saturating_sub(1));
        lines.push(Line::from(Span::styled(
            format!("  +{hidden} more"),
            panel_muted_style(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(panel_style())
            .wrap(Wrap { trim: false }),
        content_area,
    );
}

fn compact_context_lines(
    snapshot: Option<&ClientSnapshot>,
    selected: &SessionView,
    context_tokens: u64,
) -> Vec<Line<'static>> {
    let children = direct_children(snapshot, selected);
    let routines = associated_routines(snapshot, selected);
    let mut lines = vec![
        Line::from(Span::styled("Session Context", THEME.accent_bold())),
        usage_line(selected, context_tokens),
        compact_section_line(
            "Subtasks",
            children.first().map_or_else(
                || empty_line("No direct subtasks."),
                |child| subtask_line(child),
            ),
        ),
        compact_section_line(
            "Routines",
            routines.first().map_or_else(
                || empty_line("No associated Routines."),
                |summary| routine_line(summary),
            ),
        ),
    ];
    let hidden = children
        .len()
        .saturating_sub(1)
        .saturating_add(routines.len().saturating_sub(1));
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("  +{hidden} more"),
            panel_muted_style(),
        )));
    }
    lines
}

fn compact_section_line(title: &'static str, content: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled(title, panel_muted_style()), Span::raw("  ")];
    spans.extend(content.spans);
    Line::from(spans)
}

fn context_lines(
    snapshot: Option<&ClientSnapshot>,
    selected: &SessionView,
    context_tokens: u64,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Session Context", THEME.accent_bold())),
        Line::default(),
        Line::from(Span::styled("Usage", panel_muted_style())),
        Line::from(vec![
            Span::styled("  Context  ", panel_muted_style()),
            Span::raw(format_count(context_tokens)),
        ]),
        Line::from(vec![
            Span::styled("  Cache  ", panel_muted_style()),
            Span::raw(format!(
                "read {}  write {}",
                cache_rate(
                    selected.metrics.cache_read_tokens,
                    selected.metrics.total_input_tokens
                ),
                cache_rate(
                    selected.metrics.cache_write_tokens,
                    selected.metrics.total_input_tokens
                )
            )),
        ]),
        Line::from(vec![
            Span::styled("  Cost  ", panel_muted_style()),
            Span::raw(format_compact_currency_cost(
                selected.metrics.total_cost_micros,
                selected.metrics.cost_currency.as_deref(),
            )),
        ]),
        Line::default(),
        Line::from(Span::styled("Subtasks", panel_muted_style())),
    ];

    let children = direct_children(snapshot, selected);
    for child in &children {
        lines.push(subtask_line(child));
    }
    if children.is_empty() {
        lines.push(empty_line("No direct subtasks."));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Routines", panel_muted_style())));
    let routines = associated_routines(snapshot, selected);
    if routines.is_empty() {
        lines.push(empty_line("No associated Routines."));
    } else {
        lines.extend(routines.iter().map(|summary| routine_line(summary)));
    }
    lines
}

fn direct_children<'a>(
    snapshot: Option<&'a ClientSnapshot>,
    selected: &SessionView,
) -> Vec<&'a SessionView> {
    snapshot
        .map_or(&[][..], |snapshot| snapshot.sessions.current.as_slice())
        .iter()
        .filter(|child| {
            child.associations.task_parent_session_id.as_deref() == Some(selected.id.as_str())
        })
        .collect()
}

fn usage_line(session: &SessionView, context_tokens: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled("Usage  ", panel_muted_style()),
        Span::styled(
            format!("ctx {}", format_count(context_tokens)),
            panel_style(),
        ),
        Span::styled("  cache ", panel_muted_style()),
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
            panel_style(),
        ),
        Span::styled("  cost ", panel_muted_style()),
        Span::styled(
            format_compact_currency_cost(
                session.metrics.total_cost_micros,
                session.metrics.cost_currency.as_deref(),
            ),
            panel_style(),
        ),
    ])
}

fn subtask_line(session: &SessionView) -> Line<'static> {
    let label = session
        .associations
        .task_description
        .as_deref()
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| session.display_label());
    let (status, style) = subtask_status(session);
    Line::from(vec![
        Span::styled("  ", panel_style()),
        Span::styled("•", style),
        Span::raw(" "),
        Span::raw(label.to_owned()),
        Span::styled("  ", panel_muted_style()),
        Span::styled(status, style),
    ])
}

fn subtask_status(session: &SessionView) -> (String, Style) {
    if let Some(status) = progress_coordination_status(session) {
        return (status, THEME.status_style(StatusRole::Loading));
    }
    let text = match session.progress.phase {
        SessionProgressPhaseView::Unknown => "unknown",
        SessionProgressPhaseView::Idle => "idle",
        SessionProgressPhaseView::Running => match session.progress.run_stage {
            Some(SessionRunStageView::BuildingRequest) => "building request",
            Some(SessionRunStageView::StreamingProvider) => "streaming provider",
            Some(SessionRunStageView::RetryingProvider) => "retrying provider",
            Some(SessionRunStageView::EvaluatingToolCall) => "evaluating tool call",
            Some(SessionRunStageView::RunningTool) => "running tool",
            Some(SessionRunStageView::Unknown) | None => "running",
        },
        SessionProgressPhaseView::AwaitingToolResults => "awaiting tool results",
        SessionProgressPhaseView::AwaitingExternalInput => "awaiting external input",
        SessionProgressPhaseView::Interrupted => "interrupted",
        SessionProgressPhaseView::Finished => "finished",
        SessionProgressPhaseView::Failed => "failed",
        SessionProgressPhaseView::CompletionBarrier | SessionProgressPhaseView::Paused => {
            unreachable!("coordination progress handled above")
        }
    };
    let style = match session.progress.phase {
        SessionProgressPhaseView::Running
        | SessionProgressPhaseView::AwaitingToolResults
        | SessionProgressPhaseView::AwaitingExternalInput => THEME.status_style(StatusRole::Ready),
        SessionProgressPhaseView::Failed | SessionProgressPhaseView::Interrupted => {
            THEME.status_style(StatusRole::Error)
        }
        _ => panel_muted_style(),
    };
    (text.to_owned(), style)
}

fn associated_routines<'a>(
    snapshot: Option<&'a ClientSnapshot>,
    selected: &SessionView,
) -> &'a [ClientRoutineInvocationSummary] {
    snapshot
        .and_then(|snapshot| {
            snapshot
                .routines
                .session_histories
                .get(&selected.id)
                .map(|history| &history.recent)
        })
        .map_or(&[], Vec::as_slice)
}

fn routine_line(summary: &ClientRoutineInvocationSummary) -> Line<'static> {
    let (status, style) = match summary.status {
        ClientRoutineInvocationStatus::Admitted => {
            ("admitted", THEME.status_style(StatusRole::Ready))
        }
        ClientRoutineInvocationStatus::Running => {
            ("running", THEME.status_style(StatusRole::Ready))
        }
        ClientRoutineInvocationStatus::Succeeded => ("succeeded", panel_muted_style()),
        ClientRoutineInvocationStatus::Failed => ("failed", THEME.status_style(StatusRole::Error)),
        ClientRoutineInvocationStatus::Interrupted => {
            ("interrupted", THEME.status_style(StatusRole::Error))
        }
    };
    Line::from(vec![
        Span::styled("  •", style),
        Span::raw(format!(
            " {} [{}]  ",
            summary.routine_name, summary.invocation_id
        )),
        Span::styled(status, style),
    ])
}

fn empty_line(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(format!("  {text}"), panel_muted_style()))
}

#[cfg(test)]
mod tests {
    use waltr_client::{
        SessionAssociationsView, SessionCapabilitiesView, SessionCoordinationView,
        SessionMetricsView, SessionPhaseView, SessionProgressView, SessionStatusView,
    };

    use super::*;

    #[test]
    fn subtask_status_projects_exact_suspension_labels_and_semantic_colors() {
        for (phase, label, color) in [
            (
                SessionProgressPhaseView::AwaitingToolResults,
                "awaiting tool results",
                THEME.success(),
            ),
            (
                SessionProgressPhaseView::AwaitingExternalInput,
                "awaiting external input",
                THEME.success(),
            ),
            (
                SessionProgressPhaseView::Interrupted,
                "interrupted",
                THEME.danger(),
            ),
            (
                SessionProgressPhaseView::Unknown,
                "unknown",
                THEME.muted_text(),
            ),
        ] {
            let mut session = session(SessionPhaseView::Suspended);
            session.progress.phase = phase;
            let (actual_label, style) = subtask_status(&session);
            assert_eq!(actual_label, label);
            assert_eq!(style.fg, Some(color));
        }
    }

    #[test]
    fn subtask_and_routine_status_buckets_use_ready_error_and_muted_roles() {
        for (phase, label, color) in [
            (
                SessionPhaseView::RunningTool,
                "running tool",
                THEME.success(),
            ),
            (SessionPhaseView::Failed, "failed", THEME.danger()),
            (SessionPhaseView::Finished, "finished", THEME.muted_text()),
            (SessionPhaseView::Unknown, "unknown", THEME.muted_text()),
        ] {
            let (actual_label, style) = subtask_status(&session(phase));
            assert_eq!(actual_label, label);
            assert_eq!(style.fg, Some(color));
        }

        for (status, label, color) in [
            (
                ClientRoutineInvocationStatus::Admitted,
                "admitted",
                THEME.success(),
            ),
            (
                ClientRoutineInvocationStatus::Running,
                "running",
                THEME.success(),
            ),
            (
                ClientRoutineInvocationStatus::Succeeded,
                "succeeded",
                THEME.muted_text(),
            ),
            (
                ClientRoutineInvocationStatus::Failed,
                "failed",
                THEME.danger(),
            ),
            (
                ClientRoutineInvocationStatus::Interrupted,
                "interrupted",
                THEME.danger(),
            ),
        ] {
            let summary = summary(status);
            let line = routine_line(&summary);
            let status_span = line.spans.last().expect("Routine status span");
            assert_eq!(status_span.content, label);
            assert_eq!(status_span.style.fg, Some(color));
        }
    }

    fn session(phase: SessionPhaseView) -> SessionView {
        SessionView {
            id: "session".to_owned(),
            agent_name: "default".to_owned(),
            revision: 1,
            status: SessionStatusView {
                phase,
                suspension_kind: None,
                failed_message: String::new(),
                authentication_failure: None,
            },
            coordination: SessionCoordinationView::default(),
            progress: SessionProgressView {
                phase: match phase {
                    SessionPhaseView::RunningTool => SessionProgressPhaseView::Running,
                    SessionPhaseView::Finished => SessionProgressPhaseView::Finished,
                    SessionPhaseView::Failed => SessionProgressPhaseView::Failed,
                    _ => SessionProgressPhaseView::Unknown,
                },
                run_stage: (phase == SessionPhaseView::RunningTool)
                    .then_some(SessionRunStageView::RunningTool),
                ..Default::default()
            },
            capabilities: SessionCapabilitiesView::default(),
            current_prompt: None,
            created_at: None,
            updated_at: None,
            last_activity_at: None,
            inference_settings: None,
            metrics: SessionMetricsView::default(),
            associations: SessionAssociationsView::default(),
            metadata: Default::default(),
        }
    }

    fn summary(status: ClientRoutineInvocationStatus) -> ClientRoutineInvocationSummary {
        ClientRoutineInvocationSummary {
            admission_sequence: 1,
            invocation_id: "invocation".to_owned(),
            routine_name: "routine".to_owned(),
            routine_revision: "revision".to_owned(),
            trigger_name: "manual".to_owned(),
            trigger_kind: waltr_client::ClientRoutineTriggerKind::Manual,
            correlation_id: "correlation".to_owned(),
            metadata: Default::default(),
            source_cursor: None,
            status,
            admitted_at_ms: 1,
            started_at_ms: None,
            completed_at_ms: None,
            updated_at_ms: 1,
            terminal_error: None,
            latest_sequence: 1,
        }
    }
}
