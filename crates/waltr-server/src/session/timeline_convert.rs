use waltr::grpc::waltr::session::v1::{
    ContentRefTimelineContent, EmptyTimelineContent, ExternalRequestTimelineContent,
    JsonTimelineContent, ProjectedTimelineEvent as ProtoProjectedTimelineEvent,
    SyntheticTimelineEvent as ProtoSyntheticTimelineEvent, TextDeltaTimelineContent,
    TextSnapshotTimelineContent, TimelineContent as ProtoTimelineContent,
    TimelineDirection as ProtoTimelineDirection, TimelineEvent as ProtoTimelineEvent,
    TimelineEventKind as ProtoTimelineEventKind, TimelinePresentation as ProtoTimelinePresentation,
    TimelineProjectionMetadata as ProtoTimelineProjectionMetadata,
    TimelineQuery as ProtoTimelineQuery, TimelineStatus as ProtoTimelineStatus,
    ToolCallTimelineContent, ToolResultTimelineContent, projected_timeline_event, timeline_content,
};
use waltr_agent::{
    AgentTimelineEvent, SessionSyntheticTimelineEvent, SessionTimelinePage,
    SessionTimelinePresentation, SessionTimelineProjectedEvent, SessionTimelineProjectionEvent,
    TimelineChannel, TimelineContent, TimelineCursor, TimelineDirection as AgentTimelineDirection,
    TimelineEventKind, TimelineQuery, TimelineStatus,
};
use tonic::Status;

use super::convert::external_request_to_proto;

const DEFAULT_TIMELINE_LIMIT: usize = 100;
const MAX_TIMELINE_LIMIT: usize = 1000;

pub(super) fn query_from_proto(query: Option<ProtoTimelineQuery>) -> TimelineQuery {
    let Some(query) = query else {
        return TimelineQuery::backward(None, DEFAULT_TIMELINE_LIMIT);
    };
    let direction = match ProtoTimelineDirection::try_from(query.direction)
        .unwrap_or(ProtoTimelineDirection::Unspecified)
    {
        ProtoTimelineDirection::Forward => AgentTimelineDirection::Forward,
        ProtoTimelineDirection::Backward | ProtoTimelineDirection::Unspecified => {
            AgentTimelineDirection::Backward
        }
    };
    let cursor = query.cursor.map(TimelineCursor::new);
    let limit = limit_or_default(query.limit);
    let channels = query.channels;
    let query = TimelineQuery {
        direction,
        cursor,
        limit,
        channels: None,
    };
    with_channel_filter(query, query_channels(channels))
}

pub(super) fn catchup_query(
    cursor: Option<u64>,
    limit: usize,
    channels: &[String],
) -> TimelineQuery {
    with_channel_filter(
        TimelineQuery::forward(cursor.map(TimelineCursor::new), limit),
        query_channels(channels.to_vec()),
    )
}

pub(super) fn initial_query(limit: u32, channels: &[String]) -> TimelineQuery {
    with_channel_filter(
        TimelineQuery::backward(None, limit_or_default(limit)),
        query_channels(channels.to_vec()),
    )
}

pub(super) fn channel_matches(event: &AgentTimelineEvent, channels: &[String]) -> bool {
    channels.is_empty()
        || event
            .channel
            .as_ref()
            .is_some_and(|channel| channels.iter().any(|expected| expected == channel.as_str()))
}

pub(super) fn timeline_event_to_proto(
    event: AgentTimelineEvent,
) -> Result<ProtoTimelineEvent, Status> {
    Ok(ProtoTimelineEvent {
        cursor: event.cursor.get(),
        session_id: event.session_id.as_str().to_owned(),
        turn: event.turn,
        item_id: event.item_id.as_str().to_owned(),
        parent_id: event.parent_id.map(|id| id.as_str().to_owned()),
        kind: timeline_kind_to_proto(event.kind) as i32,
        channel: event.channel.map(|channel| channel.as_str().to_owned()),
        content: Some(timeline_content_to_proto(event.content)?),
        status: event
            .status
            .map(|status| timeline_status_to_proto(status) as i32),
        metadata_json: serde_json::to_string(&event.metadata)
            .map_err(|error| Status::internal(error.to_string()))?,
    })
}

pub(super) fn timeline_page_to_proto(
    page: SessionTimelinePage,
) -> Result<
    (
        Vec<ProtoProjectedTimelineEvent>,
        ProtoTimelineProjectionMetadata,
    ),
    Status,
> {
    let events = page
        .events
        .into_iter()
        .map(projected_timeline_event_to_proto)
        .collect::<Result<_, _>>()?;
    let metadata = ProtoTimelineProjectionMetadata {
        revision: page.projection_revision.get(),
        raw_page_boundary: page.raw_page_boundary.map(TimelineCursor::get),
        has_more: page.has_more,
    };
    Ok((events, metadata))
}

pub(super) fn projected_timeline_event_to_proto(
    projected: SessionTimelineProjectionEvent,
) -> Result<ProtoProjectedTimelineEvent, Status> {
    let event = match projected.event {
        SessionTimelineProjectedEvent::Durable(event) => {
            projected_timeline_event::Event::Durable(timeline_event_to_proto(event)?)
        }
        SessionTimelineProjectedEvent::Synthetic(event) => {
            projected_timeline_event::Event::Synthetic(synthetic_timeline_event_to_proto(event)?)
        }
    };
    let (presentation, mask_id) = match projected.presentation {
        SessionTimelinePresentation::Durable => (ProtoTimelinePresentation::Durable, None),
        SessionTimelinePresentation::MaskReplacement { mask_id } => (
            ProtoTimelinePresentation::MaskReplacement,
            Some(mask_id.as_str().to_owned()),
        ),
    };
    Ok(ProtoProjectedTimelineEvent {
        event: Some(event),
        presentation: presentation as i32,
        mask_id,
    })
}

fn synthetic_timeline_event_to_proto(
    event: SessionSyntheticTimelineEvent,
) -> Result<ProtoSyntheticTimelineEvent, Status> {
    Ok(ProtoSyntheticTimelineEvent {
        cursor: event.cursor.get(),
        session_id: event.session_id.as_str().to_owned(),
        turn: event.turn,
        kind: timeline_kind_to_proto(event.kind) as i32,
        channel: event.channel.as_str().to_owned(),
        content: Some(timeline_content_to_proto(event.content)?),
        status: timeline_status_to_proto(event.status) as i32,
    })
}

fn timeline_content_to_proto(content: TimelineContent) -> Result<ProtoTimelineContent, Status> {
    let content = match content {
        TimelineContent::Empty => timeline_content::Content::Empty(EmptyTimelineContent {}),
        TimelineContent::TextDelta { delta } => {
            timeline_content::Content::TextDelta(TextDeltaTimelineContent { delta })
        }
        TimelineContent::TextSnapshot { text } => {
            timeline_content::Content::TextSnapshot(TextSnapshotTimelineContent { text })
        }
        TimelineContent::ToolCall { call } => {
            timeline_content::Content::ToolCall(ToolCallTimelineContent {
                id: call.id,
                name: call.name,
                arguments_json: serde_json::to_string(&call.arguments)
                    .map_err(|error| Status::internal(error.to_string()))?,
            })
        }
        TimelineContent::ToolResult { result } => {
            timeline_content::Content::ToolResult(ToolResultTimelineContent {
                id: result.call_id,
                name: result.name,
                content: result.content.to_string(),
                error: result.is_error,
            })
        }
        TimelineContent::ExternalRequest { request } => {
            timeline_content::Content::ExternalRequest(ExternalRequestTimelineContent {
                request: Some(external_request_to_proto(request)),
            })
        }
        TimelineContent::ContentRef { content_id, bytes } => {
            timeline_content::Content::ContentRef(ContentRefTimelineContent {
                content_id: content_id.as_str().to_owned(),
                bytes,
            })
        }
        TimelineContent::Json { value } => timeline_content::Content::Json(JsonTimelineContent {
            value_json: serde_json::to_string(&value)
                .map_err(|error| Status::internal(error.to_string()))?,
        }),
    };

    Ok(ProtoTimelineContent {
        content: Some(content),
    })
}

const fn timeline_kind_to_proto(kind: TimelineEventKind) -> ProtoTimelineEventKind {
    match kind {
        TimelineEventKind::ItemStarted => ProtoTimelineEventKind::ItemStarted,
        TimelineEventKind::ContentDelta => ProtoTimelineEventKind::ContentDelta,
        TimelineEventKind::ItemUpdated => ProtoTimelineEventKind::ItemUpdated,
        TimelineEventKind::ItemFinished => ProtoTimelineEventKind::ItemFinished,
        TimelineEventKind::ExternalRequestOpened => ProtoTimelineEventKind::ExternalRequestOpened,
        TimelineEventKind::ExternalRequestResolved => {
            ProtoTimelineEventKind::ExternalRequestResolved
        }
        TimelineEventKind::SessionStatusChanged => ProtoTimelineEventKind::SessionStatusChanged,
    }
}

const fn timeline_status_to_proto(status: TimelineStatus) -> ProtoTimelineStatus {
    match status {
        TimelineStatus::Idle => ProtoTimelineStatus::Idle,
        TimelineStatus::BuildingRequest => ProtoTimelineStatus::BuildingRequest,
        TimelineStatus::StreamingProvider => ProtoTimelineStatus::StreamingProvider,
        TimelineStatus::RetryingProvider => ProtoTimelineStatus::RetryingProvider,
        TimelineStatus::EvaluatingToolCall => ProtoTimelineStatus::EvaluatingToolCall,
        TimelineStatus::RunningTool => ProtoTimelineStatus::RunningTool,
        TimelineStatus::Pending => ProtoTimelineStatus::Pending,
        TimelineStatus::AwaitingToolResults => ProtoTimelineStatus::AwaitingToolResults,
        TimelineStatus::AwaitingExternalInput => ProtoTimelineStatus::AwaitingExternalInput,
        TimelineStatus::Interrupted => ProtoTimelineStatus::Interrupted,
        TimelineStatus::Resolved => ProtoTimelineStatus::Resolved,
        TimelineStatus::Finished => ProtoTimelineStatus::Finished,
        TimelineStatus::Failed => ProtoTimelineStatus::Failed,
        TimelineStatus::Canceled => ProtoTimelineStatus::Canceled,
    }
}

fn limit_or_default(limit: u32) -> usize {
    if limit == 0 {
        DEFAULT_TIMELINE_LIMIT
    } else {
        usize::try_from(limit)
            .unwrap_or(MAX_TIMELINE_LIMIT)
            .min(MAX_TIMELINE_LIMIT)
    }
}

fn query_channels(channels: Vec<String>) -> Vec<TimelineChannel> {
    channels
        .into_iter()
        .filter(|channel| !channel.trim().is_empty())
        .map(TimelineChannel::new)
        .collect()
}

fn with_channel_filter(mut query: TimelineQuery, channels: Vec<TimelineChannel>) -> TimelineQuery {
    if !channels.is_empty() {
        query.channels = Some(channels);
    }
    query
}

#[cfg(test)]
mod tests {
    use waltr::grpc::waltr::session::v1::timeline_content;
    use waltr_agent::{
        TimelineContent, ToolResult, ToolResultContent, ToolResultContentBlock, ToolResultImage,
    };

    use super::timeline_content_to_proto;

    #[test]
    fn tool_result_projection_omits_image_data_and_preserves_error_state() {
        let result = ToolResult::from_content(
            "call-image",
            "camera",
            ToolResultContent::from_blocks(vec![ToolResultContentBlock::Image(
                ToolResultImage::new("iVBORw0KGgo=", "image/png").expect("valid image"),
            )])
            .expect("bounded content"),
            true,
        );

        let projected = timeline_content_to_proto(TimelineContent::ToolResult { result })
            .expect("projected tool result")
            .content
            .expect("timeline content");
        let timeline_content::Content::ToolResult(projected) = projected else {
            panic!("expected tool result projection");
        };

        assert!(projected.error);
        assert!(projected.content.contains("image/png"));
        assert!(projected.content.contains("8 bytes"));
        assert!(!projected.content.contains("iVBOR"));
    }
}
