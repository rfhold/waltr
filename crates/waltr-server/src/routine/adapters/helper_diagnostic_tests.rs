use super::*;
use std::io::Write;
use waltr_agent::{AgentSessionRuntimeError, AuthenticationFailureReason, LlmError};

const SECRET: &str = "sentinel-token-transcript-https://private.invalid/?token=secret";

#[test]
fn completion_match_only_equates_absent_text_with_empty_response() {
    let id = SessionId::generate();
    for (text, response) in [
        (None, ""),
        (Some(""), ""),
        (Some("done"), "done"),
        (Some(" \n"), " \n"),
    ] {
        assert_eq!(match_completion_response(text, response, &id), Ok(()));
    }
    for (text, response) in [
        (None, "done"),
        (Some(""), "done"),
        (Some("done"), ""),
        (Some("done"), "other"),
        (Some(" done"), "done"),
        (Some("\n"), ""),
    ] {
        let logs = capture_logs(|| {
            assert_eq!(
                match_completion_response(text, response, &id),
                Err(PortError::Uncertain)
            );
        });
        assert!(logs.contains("stage=\"completion_match\""), "{logs}");
        assert!(logs.contains("category=\"response_mismatch\""), "{logs}");
    }
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs(action: impl FnOnce()) -> String {
    let buffer = LogBuffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, action);
    let bytes = buffer.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn runtime_diagnostics_classify_typed_errors_without_message_or_context() {
    use AuthenticationFailureReason::*;
    let mut cases = vec![
        (
            AgentSessionRuntimeError::FailedPrecondition(SECRET.into()),
            "runtime_precondition",
        ),
        (
            AgentSessionRuntimeError::Internal(SECRET.into()),
            "runtime_internal",
        ),
        (
            AgentSessionRuntimeError::Agent(LlmError::Protocol {
                provider: SECRET,
                message: SECRET.into(),
            }),
            "protocol",
        ),
        (
            AgentSessionRuntimeError::Agent(LlmError::Authorization {
                provider: SECRET,
                message: SECRET.into(),
                status: Some(401),
            }),
            "authorization",
        ),
        (
            AgentSessionRuntimeError::Agent(LlmError::Api {
                provider: SECRET,
                message: SECRET.into(),
                status: Some(500),
                headers: vec![(SECRET.into(), SECRET.into())],
                code: Some(SECRET.into()),
                body: Some(SECRET.into()),
            }),
            "provider_api",
        ),
        (
            AgentSessionRuntimeError::Agent(LlmError::MissingConfig { name: SECRET }),
            "missing_config",
        ),
        (
            AgentSessionRuntimeError::Agent(LlmError::Io(std::io::Error::other(SECRET))),
            "io",
        ),
    ];
    for (reason, category) in [
        (CredentialRequired, "auth_credential_required"),
        (CredentialRejected, "auth_credential_rejected"),
        (TokenExchangeFailed, "auth_token_exchange_failed"),
        (NetworkFailed, "auth_network_failed"),
        (ResponseInvalid, "auth_response_invalid"),
        (ConfigurationRequired, "auth_configuration_required"),
    ] {
        cases.push((
            AgentSessionRuntimeError::Authentication {
                target: waltr_agent::AuthTarget::Host {
                    namespace: SECRET.into(),
                    key: SECRET.into(),
                },
                message: SECRET.into(),
                reason,
                host_context: Some(json!({"secret": SECRET})),
            },
            category,
        ));
    }
    let id = SessionId::generate();
    for (error, category) in cases {
        let logs = capture_logs(|| {
            let result: Result<(), PortError> = Err(error).map_err(|error| {
                helper_failure(
                    "runtime_assembly",
                    runtime_category(&error),
                    Some(&id),
                    PortError::Unavailable,
                )
            });
            assert_eq!(result, Err(PortError::Unavailable));
        });
        assert!(logs.contains("routine helper failed"), "{logs}");
        assert!(logs.contains("stage=\"runtime_assembly\""), "{logs}");
        assert!(logs.contains(&format!("category=\"{category}\"")), "{logs}");
        assert!(logs.contains(id.as_str()), "{logs}");
        assert!(!logs.contains(SECRET), "{logs}");
        assert!(!logs.contains("private.invalid"), "{logs}");
    }
}

#[test]
fn owned_run_diagnostics_preserve_uncertain_and_expose_only_available_categories() {
    let id = SessionId::generate();
    for (error, category) in [
        (
            waltr_agent::OwnedExecutionError::Unavailable,
            "owned_unavailable",
        ),
        (
            waltr_agent::OwnedExecutionError::Uncertain,
            "owned_uncertain",
        ),
    ] {
        let logs = capture_logs(|| {
            assert_eq!(owned_failure(error, &id), PortError::Uncertain);
        });
        assert!(logs.contains("stage=\"run\""), "{logs}");
        assert!(logs.contains(&format!("category=\"{category}\"")), "{logs}");
        assert!(logs.contains(id.as_str()), "{logs}");
    }
}

#[test]
fn status_diagnostics_omit_failure_and_retry_messages() {
    let id = SessionId::generate();
    for (status, expected) in [
        (
            SessionStatus::Failed {
                message: SECRET.into(),
                authentication_failure: None,
            },
            "failed",
        ),
        (
            SessionStatus::Failed {
                message: SECRET.into(),
                authentication_failure: Some(waltr_agent::AuthenticationFailure {
                    target: waltr_agent::AuthTarget::provider(SECRET),
                    reason: AuthenticationFailureReason::CredentialRejected,
                    host_context: Some(json!({"secret": SECRET})),
                }),
            },
            "auth_credential_rejected",
        ),
        (
            SessionStatus::RetryingProvider {
                attempt: 1,
                max_attempts: 2,
                delay_ms: 1,
                message: SECRET.into(),
            },
            "retrying_provider",
        ),
    ] {
        let logs = capture_logs(|| {
            assert_eq!(
                helper_failure(
                    "run_status",
                    status_category(&status),
                    Some(&id),
                    PortError::Uncertain
                ),
                PortError::Uncertain
            );
        });
        assert!(logs.contains("stage=\"run_status\""), "{logs}");
        assert!(logs.contains(&format!("category=\"{expected}\"")), "{logs}");
        assert!(!logs.contains(SECRET), "{logs}");
    }
}

#[test]
fn diagnostic_stages_leave_original_port_errors_unchanged() {
    let id = SessionId::generate();
    for (stage, category, error) in [
        ("transient_input", "protocol", PortError::Invalid),
        ("runtime_assembly", "cancelled", PortError::Unavailable),
        ("runtime_assembly", "deadline", PortError::Unavailable),
        (
            "completion_acceptance",
            "missing_acceptance",
            PortError::Uncertain,
        ),
        (
            "completion_match",
            "response_mismatch",
            PortError::Uncertain,
        ),
        ("run_origin", "origin_ended", PortError::Uncertain),
    ] {
        let logs = capture_logs(|| {
            assert_eq!(helper_failure(stage, category, Some(&id), error), error);
        });
        assert!(logs.contains(&format!("stage=\"{stage}\"")), "{logs}");
        assert!(logs.contains(&format!("category=\"{category}\"")), "{logs}");
    }
}
