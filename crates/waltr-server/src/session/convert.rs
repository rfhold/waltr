use std::{collections::BTreeMap, time::SystemTime};

use super::{
    error::authentication_failure_to_proto, inference_settings::inference_settings_to_proto,
};
use waltr::grpc::waltr::{
    config::v1::Configuration,
    session::v1::{
        ExternalRequest as ProtoExternalRequest, ExternalRequestKind as ProtoExternalRequestKind,
        PendingWork as ProtoPendingWork, PendingWorkStatus as ProtoPendingWorkStatus,
        SessionCapabilities as ProtoSessionCapabilities,
        SessionCoordinationPhase as ProtoSessionCoordinationPhase,
        SessionCoordinationReason as ProtoSessionCoordinationReason,
        SessionCoordinationState as ProtoSessionCoordinationState,
        SessionMetrics as ProtoSessionMetrics, SessionPhase as ProtoSessionPhase,
        SessionProgress as ProtoSessionProgress, SessionProgressPhase as ProtoSessionProgressPhase,
        SessionPromptDisposition as ProtoSessionPromptDisposition,
        SessionRunStage as ProtoSessionRunStage, SessionStatus as ProtoSessionStatus,
        SessionSummary as ProtoSessionSummary, Suspension as ProtoSuspension,
        SuspensionKind as ProtoSuspensionKind,
    },
};
use waltr_agent::{
    ExternalRequest, ExternalRequestKind, PendingWork, PendingWorkStatus, SessionCapabilities,
    SessionCoordinationPhase, SessionCoordinationReason, SessionCoordinationState, SessionProgress,
    SessionProgressPhase, SessionPromptDisposition, SessionRunStage, SessionStatus, SessionSummary,
    Suspension,
};

pub(super) fn session_summary_to_proto(
    summary: SessionSummary,
    config: &Configuration,
    default_agent_name: &str,
) -> Result<ProtoSessionSummary, tonic::Status> {
    let inference_settings = summary
        .inference_settings
        .map(|settings| inference_settings_to_proto(settings, config));
    let metadata = metadata_to_proto(summary.metadata_values)?;
    Ok(ProtoSessionSummary {
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
        metadata: Some(metadata),
        coordination: Some(session_coordination_to_proto(summary.coordination)),
        progress: Some(session_progress_to_proto(summary.progress)),
        capabilities: Some(session_capabilities_to_proto(summary.capabilities)),
    })
}

fn session_progress_to_proto(progress: SessionProgress) -> ProtoSessionProgress {
    ProtoSessionProgress {
        phase: match progress.phase {
            SessionProgressPhase::Unknown => ProtoSessionProgressPhase::Unspecified,
            SessionProgressPhase::Idle => ProtoSessionProgressPhase::Idle,
            SessionProgressPhase::Running => ProtoSessionProgressPhase::Running,
            SessionProgressPhase::CompletionBarrier => ProtoSessionProgressPhase::CompletionBarrier,
            SessionProgressPhase::Paused => ProtoSessionProgressPhase::Paused,
            SessionProgressPhase::AwaitingToolResults => {
                ProtoSessionProgressPhase::AwaitingToolResults
            }
            SessionProgressPhase::AwaitingExternalInput => {
                ProtoSessionProgressPhase::AwaitingExternalInput
            }
            SessionProgressPhase::Interrupted => ProtoSessionProgressPhase::Interrupted,
            SessionProgressPhase::Finished => ProtoSessionProgressPhase::Finished,
            SessionProgressPhase::Failed => ProtoSessionProgressPhase::Failed,
        } as i32,
        run_stage: progress.run_stage.map(|stage| match stage {
            SessionRunStage::BuildingRequest => ProtoSessionRunStage::BuildingRequest as i32,
            SessionRunStage::StreamingProvider => ProtoSessionRunStage::StreamingProvider as i32,
            SessionRunStage::RetryingProvider => ProtoSessionRunStage::RetryingProvider as i32,
            SessionRunStage::EvaluatingToolCall => ProtoSessionRunStage::EvaluatingToolCall as i32,
            SessionRunStage::RunningTool => ProtoSessionRunStage::RunningTool as i32,
        }),
        reason: progress.reason.map(session_coordination_reason_to_proto),
        queued_count: u64::try_from(progress.queued_count).unwrap_or(u64::MAX),
        continuation_armed: progress.continuation_armed,
        running: progress.running,
    }
}

fn session_capabilities_to_proto(capabilities: SessionCapabilities) -> ProtoSessionCapabilities {
    ProtoSessionCapabilities {
        prompt_disposition: match capabilities.prompt_disposition {
            SessionPromptDisposition::Immediate => ProtoSessionPromptDisposition::Immediate,
            SessionPromptDisposition::Queued => ProtoSessionPromptDisposition::Queued,
            SessionPromptDisposition::SupersedeSuspension => {
                ProtoSessionPromptDisposition::SupersedeSuspension
            }
        } as i32,
        can_respond_external_request: capabilities.can_respond_external_request,
        can_interrupt: capabilities.can_interrupt,
    }
}

fn session_coordination_to_proto(state: SessionCoordinationState) -> ProtoSessionCoordinationState {
    ProtoSessionCoordinationState {
        phase: match state.phase {
            SessionCoordinationPhase::Ready => ProtoSessionCoordinationPhase::Ready,
            SessionCoordinationPhase::Running => ProtoSessionCoordinationPhase::Running,
            SessionCoordinationPhase::CompletionBarrier => {
                ProtoSessionCoordinationPhase::CompletionBarrier
            }
            SessionCoordinationPhase::Paused => ProtoSessionCoordinationPhase::Paused,
        } as i32,
        reason: state.reason.map(session_coordination_reason_to_proto),
        queued_count: u32::try_from(state.queued_count).unwrap_or(u32::MAX),
        continuation_armed: state.continuation_armed,
    }
}

fn session_coordination_reason_to_proto(
    reason: SessionCoordinationReason,
) -> ProtoSessionCoordinationReason {
    ProtoSessionCoordinationReason {
        code: reason.code().to_owned(),
        source: reason.source().map(str::to_owned),
        trigger: reason.trigger().map(str::to_owned),
    }
}

fn metadata_to_proto(
    metadata: BTreeMap<String, serde_json::Value>,
) -> Result<prost_types::Struct, tonic::Status> {
    metadata
        .into_iter()
        .map(|(key, value)| json_value_to_proto(value).map(|value| (key, value)))
        .collect::<Result<BTreeMap<_, _>, _>>()
        .map(|fields| prost_types::Struct { fields })
}

fn json_value_to_proto(value: serde_json::Value) -> Result<prost_types::Value, tonic::Status> {
    use prost_types::value::Kind;

    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(value) => Kind::BoolValue(value),
        serde_json::Value::String(value) => Kind::StringValue(value),
        serde_json::Value::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values
                .into_iter()
                .map(json_value_to_proto)
                .collect::<Result<_, _>>()?,
        }),
        serde_json::Value::Object(values) => Kind::StructValue(prost_types::Struct {
            fields: values
                .into_iter()
                .map(|(key, value)| json_value_to_proto(value).map(|value| (key, value)))
                .collect::<Result<_, _>>()?,
        }),
        serde_json::Value::Number(number) => {
            let value = if let Some(value) = number.as_i64() {
                if !integer_is_exact_f64(value.unsigned_abs()) {
                    return Err(lossy_metadata_number(&number));
                }
                number_to_f64(&number)?
            } else if let Some(value) = number.as_u64() {
                if !integer_is_exact_f64(value) {
                    return Err(lossy_metadata_number(&number));
                }
                number_to_f64(&number)?
            } else {
                number
                    .as_f64()
                    .ok_or_else(|| lossy_metadata_number(&number))?
            };
            Kind::NumberValue(value)
        }
    };
    Ok(prost_types::Value { kind: Some(kind) })
}

const fn integer_is_exact_f64(magnitude: u64) -> bool {
    const SIGNIFICAND_BITS: u32 = 53;

    if magnitude == 0 {
        return true;
    }
    let significant_bits = u64::BITS - magnitude.leading_zeros();
    significant_bits <= SIGNIFICAND_BITS
        || magnitude.trailing_zeros() >= significant_bits - SIGNIFICAND_BITS
}

fn number_to_f64(number: &serde_json::Number) -> Result<f64, tonic::Status> {
    number
        .to_string()
        .parse()
        .map_err(|_| lossy_metadata_number(number))
}

fn lossy_metadata_number(number: &serde_json::Number) -> tonic::Status {
    tonic::Status::data_loss(format!(
        "session metadata number {number} is not losslessly representable as a protobuf double"
    ))
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
    use serde_json::json;

    #[test]
    fn progress_and_capabilities_preserve_every_agent_value() {
        let phases = [
            (
                SessionProgressPhase::Unknown,
                ProtoSessionProgressPhase::Unspecified,
            ),
            (SessionProgressPhase::Idle, ProtoSessionProgressPhase::Idle),
            (
                SessionProgressPhase::Running,
                ProtoSessionProgressPhase::Running,
            ),
            (
                SessionProgressPhase::CompletionBarrier,
                ProtoSessionProgressPhase::CompletionBarrier,
            ),
            (
                SessionProgressPhase::Paused,
                ProtoSessionProgressPhase::Paused,
            ),
            (
                SessionProgressPhase::AwaitingToolResults,
                ProtoSessionProgressPhase::AwaitingToolResults,
            ),
            (
                SessionProgressPhase::AwaitingExternalInput,
                ProtoSessionProgressPhase::AwaitingExternalInput,
            ),
            (
                SessionProgressPhase::Interrupted,
                ProtoSessionProgressPhase::Interrupted,
            ),
            (
                SessionProgressPhase::Finished,
                ProtoSessionProgressPhase::Finished,
            ),
            (
                SessionProgressPhase::Failed,
                ProtoSessionProgressPhase::Failed,
            ),
        ];
        for (phase, expected) in phases {
            let projected = session_progress_to_proto(SessionProgress {
                phase,
                running: true,
                queued_count: 4,
                continuation_armed: true,
                ..SessionProgress::default()
            });
            assert_eq!(projected.phase, expected as i32);
            assert!(projected.running);
            assert_eq!(projected.queued_count, 4);
            assert!(projected.continuation_armed);
        }

        for (stage, expected) in [
            (
                SessionRunStage::BuildingRequest,
                ProtoSessionRunStage::BuildingRequest,
            ),
            (
                SessionRunStage::StreamingProvider,
                ProtoSessionRunStage::StreamingProvider,
            ),
            (
                SessionRunStage::RetryingProvider,
                ProtoSessionRunStage::RetryingProvider,
            ),
            (
                SessionRunStage::EvaluatingToolCall,
                ProtoSessionRunStage::EvaluatingToolCall,
            ),
            (
                SessionRunStage::RunningTool,
                ProtoSessionRunStage::RunningTool,
            ),
        ] {
            let projected = session_progress_to_proto(SessionProgress {
                run_stage: Some(stage),
                ..SessionProgress::default()
            });
            assert_eq!(projected.run_stage, Some(expected as i32));
        }

        for (disposition, expected) in [
            (
                SessionPromptDisposition::Immediate,
                ProtoSessionPromptDisposition::Immediate,
            ),
            (
                SessionPromptDisposition::Queued,
                ProtoSessionPromptDisposition::Queued,
            ),
            (
                SessionPromptDisposition::SupersedeSuspension,
                ProtoSessionPromptDisposition::SupersedeSuspension,
            ),
        ] {
            let projected = session_capabilities_to_proto(SessionCapabilities {
                prompt_disposition: disposition,
                can_respond_external_request: true,
                can_interrupt: true,
            });
            assert_eq!(projected.prompt_disposition, expected as i32);
            assert!(projected.can_respond_external_request);
            assert!(projected.can_interrupt);
        }
    }

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

    #[test]
    fn session_metadata_preserves_json_values() {
        let mut record = waltr_agent::SessionRecord::new("session-metadata");
        record.set_metadata_value("null", serde_json::Value::Null);
        record.set_metadata_value("bool", json!(true));
        record.set_metadata_value("string", json!("title"));
        record.set_metadata_value("array", json!([1, false, null, { "nested": "value" }]));

        let projected =
            session_summary_to_proto(record.summary(), &Configuration::default(), "default")
                .expect("metadata projects");
        let metadata = projected.metadata.expect("metadata struct");

        assert!(matches!(
            metadata.fields["null"].kind,
            Some(prost_types::value::Kind::NullValue(_))
        ));
        assert!(matches!(
            metadata.fields["bool"].kind,
            Some(prost_types::value::Kind::BoolValue(true))
        ));
        assert!(
            matches!(metadata.fields["string"].kind, Some(prost_types::value::Kind::StringValue(ref value)) if value == "title")
        );
        assert!(matches!(
            metadata.fields["array"].kind,
            Some(prost_types::value::Kind::ListValue(_))
        ));
    }

    #[test]
    fn session_metadata_accepts_large_exact_signed_and_unsigned_integers() {
        for value in [json!(i64::MIN), json!(-(1_i64 << 54)), json!(1_u64 << 63)] {
            let projected = json_value_to_proto(value).expect("exact integer projects");
            assert!(matches!(
                projected.kind,
                Some(prost_types::value::Kind::NumberValue(_))
            ));
        }
    }

    #[test]
    fn session_metadata_rejects_adjacent_lossy_signed_and_unsigned_integers() {
        for value in [
            json!((1_i64 << 54) + 1),
            json!(-(1_i64 << 54) - 1),
            json!((1_u64 << 63) + 1),
        ] {
            let expected = value.to_string();
            let error = json_value_to_proto(value).expect_err("lossy integer must fail");

            assert_eq!(error.code(), tonic::Code::DataLoss);
            assert!(error.message().contains(&expected));
        }
    }
}
