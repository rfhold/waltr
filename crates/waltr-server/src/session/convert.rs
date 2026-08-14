use std::time::SystemTime;

use super::{
    error::authentication_failure_to_proto, inference_settings::inference_settings_to_proto,
};
use waltr::grpc::waltr::{
    config::v1::Configuration,
    session::v1::{
        ExternalRequest as ProtoExternalRequest, ExternalRequestKind as ProtoExternalRequestKind,
        PendingWork as ProtoPendingWork, PendingWorkStatus as ProtoPendingWorkStatus,
        SessionMetrics as ProtoSessionMetrics, SessionPhase as ProtoSessionPhase,
        SessionStatus as ProtoSessionStatus, SessionSummary as ProtoSessionSummary,
        Suspension as ProtoSuspension, SuspensionKind as ProtoSuspensionKind,
    },
};
use waltr_agent::{
    ExternalRequest, ExternalRequestKind, PendingWork, PendingWorkStatus, SessionStatus,
    SessionSummary, Suspension,
};

pub(super) fn session_summary_to_proto(
    summary: SessionSummary,
    config: &Configuration,
    default_agent_name: &str,
) -> ProtoSessionSummary {
    let inference_settings = summary
        .inference_settings
        .map(|settings| inference_settings_to_proto(settings, config));
    ProtoSessionSummary {
        id: summary.id.as_str().to_owned(),
        revision: summary.revision.get(),
        status: Some(session_status_to_proto(summary.status)),
        current_prompt: summary.current_prompt,
        created_at: Some(timestamp_to_proto(summary.created_at)),
        updated_at: Some(timestamp_to_proto(summary.updated_at)),
        last_activity_at: Some(timestamp_to_proto(summary.last_activity_at)),
        inference_settings,
        metrics: Some(session_metrics_to_proto(summary.metrics)),
        agent_name: summary
            .agent_name
            .unwrap_or_else(|| default_agent_name.to_owned()),
    }
}

fn session_metrics_to_proto(metrics: waltr_agent::SessionMetrics) -> ProtoSessionMetrics {
    ProtoSessionMetrics {
        input_tokens: metrics.input_tokens,
        output_tokens: metrics.output_tokens,
        cache_read_tokens: metrics.cache_read_tokens,
        cache_write_tokens: metrics.cache_write_tokens,
        image_input_tokens: metrics.image_input_tokens,
        image_input_count: metrics.image_input_count,
        total_tokens: metrics.total_tokens,
        total_input_tokens: metrics.total_input_tokens,
        context_tokens: metrics.context_tokens,
        cost_currency: metrics.cost_currency,
        total_cost_micros: metrics.total_cost_micros,
    }
}

fn session_status_to_proto(status: SessionStatus) -> ProtoSessionStatus {
    match status {
        SessionStatus::Idle => proto_status(ProtoSessionPhase::Idle),
        SessionStatus::BuildingRequest => proto_status(ProtoSessionPhase::BuildingRequest),
        SessionStatus::StreamingProvider => proto_status(ProtoSessionPhase::StreamingProvider),
        SessionStatus::RetryingProvider { .. } => proto_status(ProtoSessionPhase::RetryingProvider),
        SessionStatus::EvaluatingToolCall => proto_status(ProtoSessionPhase::EvaluatingToolCall),
        SessionStatus::RunningTool => proto_status(ProtoSessionPhase::RunningTool),
        SessionStatus::Suspended(suspension) => ProtoSessionStatus {
            phase: ProtoSessionPhase::Suspended as i32,
            suspension: Some(suspension_to_proto(suspension)),
            failed_message: String::new(),
            authentication_failure: None,
        },
        SessionStatus::Finished => proto_status(ProtoSessionPhase::Finished),
        SessionStatus::Failed {
            message,
            authentication_failure,
        } => ProtoSessionStatus {
            phase: ProtoSessionPhase::Failed as i32,
            suspension: None,
            failed_message: message,
            authentication_failure: authentication_failure.map(authentication_failure_to_proto),
        },
    }
}

const fn proto_status(phase: ProtoSessionPhase) -> ProtoSessionStatus {
    ProtoSessionStatus {
        phase: phase as i32,
        suspension: None,
        failed_message: String::new(),
        authentication_failure: None,
    }
}

fn suspension_to_proto(suspension: Suspension) -> ProtoSuspension {
    match suspension {
        Suspension::AwaitingToolResults { work } => ProtoSuspension {
            kind: ProtoSuspensionKind::AwaitingToolResults as i32,
            work: work.into_iter().map(pending_work_to_proto).collect(),
            request: None,
        },
        Suspension::AwaitingExternalInput { request } => ProtoSuspension {
            kind: ProtoSuspensionKind::AwaitingExternalInput as i32,
            work: Vec::new(),
            request: Some(external_request_to_proto(*request)),
        },
        Suspension::Interrupted { work } => ProtoSuspension {
            kind: ProtoSuspensionKind::Interrupted as i32,
            work: work.into_iter().map(pending_work_to_proto).collect(),
            request: None,
        },
    }
}

fn pending_work_to_proto(work: PendingWork) -> ProtoPendingWork {
    ProtoPendingWork {
        id: work.id,
        turn: work.turn,
        status: pending_work_status_to_proto(&work.status) as i32,
    }
}

const fn pending_work_status_to_proto(status: &PendingWorkStatus) -> ProtoPendingWorkStatus {
    match status {
        PendingWorkStatus::Pending => ProtoPendingWorkStatus::Pending,
        PendingWorkStatus::Canceled(_) => ProtoPendingWorkStatus::Canceled,
        PendingWorkStatus::Finished(_) => ProtoPendingWorkStatus::Finished,
        PendingWorkStatus::Failed { .. } => ProtoPendingWorkStatus::Failed,
    }
}

pub(super) fn external_request_to_proto(request: ExternalRequest) -> ProtoExternalRequest {
    ProtoExternalRequest {
        id: request.id.as_str().to_owned(),
        kind: external_request_kind_to_proto(&request.kind) as i32,
        tool_call_id: request.tool_call_id,
        prompt: request.prompt,
        payload_json: serde_json::to_string(&request.payload).unwrap_or_else(|_| "null".to_owned()),
    }
}

const fn external_request_kind_to_proto(kind: &ExternalRequestKind) -> ProtoExternalRequestKind {
    match kind {
        ExternalRequestKind::Permission => ProtoExternalRequestKind::Permission,
        ExternalRequestKind::Question => ProtoExternalRequestKind::Question,
        ExternalRequestKind::ToolInput => ProtoExternalRequestKind::ToolInput,
        ExternalRequestKind::Custom(_) => ProtoExternalRequestKind::Custom,
    }
}

fn timestamp_to_proto(time: SystemTime) -> prost_types::Timestamp {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(duration.subsec_nanos()).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr::grpc::waltr::auth::v1::{
        AuthenticationDomain, AuthenticationFailureReason, auth_target,
    };

    #[test]
    fn failed_session_status_preserves_structured_authentication_failure() {
        let status = session_status_to_proto(SessionStatus::Failed {
            message: "MCP credential rejected".to_owned(),
            authentication_failure: Some(waltr_agent::AuthenticationFailure {
                target: waltr_agent::AuthTarget::mcp_server("configured-mcp"),
                reason: waltr_agent::AuthenticationFailureReason::CredentialRejected,
                mcp_authorization: Some(Box::new(waltr_agent::McpAuthorizationContext {
                    endpoint: "https://mcp.example/rpc".to_owned(),
                    http_status: 401,
                    resource_metadata_url: Some(
                        "https://mcp.example/.well-known/oauth-protected-resource".to_owned(),
                    ),
                    scope: Some("tools.read".to_owned()),
                    error: Some("invalid_token".to_owned()),
                    error_description: None,
                    error_uri: None,
                    realm: None,
                })),
            }),
        });

        let failure = status
            .authentication_failure
            .expect("structured authentication failure");
        assert_eq!(
            AuthenticationDomain::try_from(failure.domain),
            Ok(AuthenticationDomain::OutboundMcp)
        );
        assert_eq!(
            AuthenticationFailureReason::try_from(failure.reason),
            Ok(AuthenticationFailureReason::CredentialRejected)
        );
        assert!(matches!(
            failure.target.and_then(|target| target.target),
            Some(auth_target::Target::McpServerKey(key)) if key == "configured-mcp"
        ));
        let context = failure.mcp_authorization.expect("MCP challenge context");
        assert_eq!(context.endpoint, "https://mcp.example/rpc");
        assert_eq!(context.scope.as_deref(), Some("tools.read"));
        assert_eq!(context.error.as_deref(), Some("invalid_token"));
    }
}
