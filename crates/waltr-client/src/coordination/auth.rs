//! Auth reducer responsibilities.

use crate::{AuthEffect, AuthEvent, ClientEffect, ClientState, ClientUpdate};

#[allow(
    clippy::too_many_lines,
    reason = "keep the auth event state machine exhaustive in one match"
)]
pub fn reduce(state: &mut ClientState, event: AuthEvent) -> ClientUpdate {
    match event {
        AuthEvent::ListRequested => {
            state.set_auth_list_loading();
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::ListStatuses))
        }
        AuthEvent::Listed(statuses) => {
            state.set_auth_list_ready(statuses);
            ClientUpdate::default()
        }
        AuthEvent::ListFailed(error) => {
            state.set_auth_list_failed(error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::BeginRequested {
            operation_key,
            target,
        } => {
            state.set_auth_begin_loading(operation_key.clone(), target.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Begin {
                operation_key,
                target,
            }))
        }
        AuthEvent::Begun {
            operation_key,
            target,
            attempt,
        } => {
            let poll = state.set_auth_begin_ready(operation_key, target, attempt);
            poll.map_or_else(
                ClientUpdate::default,
                |(attempt_id, poll_generation, delay_ms)| {
                    poll_update(attempt_id, poll_generation, delay_ms)
                },
            )
        }
        AuthEvent::PollRequested { attempt_id } => state
            .request_auth_poll(&attempt_id)
            .map_or_else(ClientUpdate::default, |(poll_generation, delay_ms)| {
                poll_update(attempt_id, poll_generation, delay_ms)
            }),
        AuthEvent::Polled {
            attempt_id,
            poll_generation,
            response,
        } => state
            .set_auth_attempt_polled(&attempt_id, poll_generation, response)
            .map_or_else(ClientUpdate::default, |delay_ms| {
                poll_update(attempt_id, poll_generation, delay_ms)
            }),
        AuthEvent::PollFailed {
            attempt_id,
            poll_generation,
            error: _,
        } => state
            .set_auth_poll_failed(&attempt_id, poll_generation)
            .map_or_else(ClientUpdate::default, |delay_ms| {
                poll_update(attempt_id, poll_generation, delay_ms)
            }),
        AuthEvent::BeginFailed {
            operation_key,
            target,
            error,
        } => {
            state.set_auth_begin_failed(operation_key, target, error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::CompleteRequested {
            operation_key,
            attempt_id,
            input,
        } => {
            state.set_auth_complete_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Complete {
                operation_key,
                attempt_id,
                input,
            }))
        }
        AuthEvent::Completed {
            operation_key,
            status,
        } => {
            state.set_auth_complete_ready(operation_key, status);
            ClientUpdate::default()
        }
        AuthEvent::CompleteFailed {
            operation_key,
            error,
        } => {
            state.set_auth_complete_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::CancelRequested {
            operation_key,
            attempt_id,
        } => {
            state.set_auth_cancel_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Cancel {
                operation_key,
                attempt_id,
            }))
        }
        AuthEvent::Canceled { operation_key } => {
            state.set_auth_cancel_ready(operation_key);
            ClientUpdate::default()
        }
        AuthEvent::CancelFailed {
            operation_key,
            error,
        } => {
            state.set_auth_cancel_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::RefreshRequested {
            operation_key,
            target,
        } => {
            state.set_auth_refresh_loading(operation_key.clone(), target.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Refresh {
                operation_key,
                target,
            }))
        }
        AuthEvent::Refreshed {
            operation_key,
            status,
        } => {
            state.set_auth_refresh_ready(operation_key, status);
            ClientUpdate::default()
        }
        AuthEvent::RefreshFailed {
            operation_key,
            error,
        } => {
            state.set_auth_refresh_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::LogoutRequested {
            operation_key,
            target,
        } => {
            state.set_auth_logout_loading(operation_key.clone(), target.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Logout {
                operation_key,
                target,
            }))
        }
        AuthEvent::LoggedOut {
            operation_key,
            status,
        } => {
            state.set_auth_logout_ready(operation_key, status);
            ClientUpdate::default()
        }
        AuthEvent::LogoutFailed {
            operation_key,
            error,
        } => {
            state.set_auth_logout_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
    }
}

fn poll_update(attempt_id: String, poll_generation: u64, delay_ms: u64) -> ClientUpdate {
    ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Poll {
        attempt_id,
        poll_generation,
        delay_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthTarget, Client, ClientEvent, GenericAuthAttemptStatus};
    use waltr::grpc::waltr::auth::v1::{
        AuthAttemptLifecycle, AuthAttemptStatus, AuthCompletionMode, BeginAuthResponse,
        McpOAuthRegistrationMethod, auth_target,
    };

    fn target(key: &str) -> AuthTarget {
        AuthTarget {
            target: Some(auth_target::Target::McpServerKey(key.to_owned())),
        }
    }

    fn begin_callback(client: &mut Client) -> (String, u64) {
        let update = client.begin_auth(target("github"));
        let [ClientEffect::Auth(AuthEffect::Begin { operation_key, .. })] = update.effects() else {
            panic!("begin effect");
        };
        let update = client.apply(ClientEvent::Auth(AuthEvent::Begun {
            operation_key: operation_key.clone(),
            target: target("github"),
            attempt: BeginAuthResponse {
                attempt_id: "attempt-1".to_owned(),
                auth_url: "https://issuer.example/authorize?state=opaque".to_owned(),
                poll_interval_seconds: 2,
                completion_mode: AuthCompletionMode::ServerCallback as i32,
                registration_method: McpOAuthRegistrationMethod::Dynamic as i32,
                ..BeginAuthResponse::default()
            },
        }));
        let [
            ClientEffect::Auth(AuthEffect::Poll {
                attempt_id,
                poll_generation,
                delay_ms,
            }),
        ] = update.effects()
        else {
            panic!("poll effect");
        };
        assert_eq!(attempt_id, "attempt-1");
        assert_eq!(*delay_ms, 2_000);
        (attempt_id.clone(), *poll_generation)
    }

    fn status(lifecycle: AuthAttemptLifecycle, target: AuthTarget) -> GenericAuthAttemptStatus {
        GenericAuthAttemptStatus {
            attempt: Some(AuthAttemptStatus {
                attempt_id: "attempt-1".to_owned(),
                target: Some(target),
                lifecycle: lifecycle as i32,
                completion_mode: AuthCompletionMode::ServerCallback as i32,
                registration_method: McpOAuthRegistrationMethod::Dynamic as i32,
                ..AuthAttemptStatus::default()
            }),
            status: None,
        }
    }

    #[test]
    fn callback_begin_polls_until_terminal_and_ignores_duplicate_terminal_events() {
        let mut client = Client::new();
        let (attempt_id, poll_generation) = begin_callback(&mut client);
        let update = client.apply(ClientEvent::Auth(AuthEvent::Polled {
            attempt_id: attempt_id.clone(),
            poll_generation,
            response: status(AuthAttemptLifecycle::Completing, target("github")),
        }));
        assert!(matches!(
            update.effects(),
            [ClientEffect::Auth(AuthEffect::Poll { .. })]
        ));

        let terminal = ClientEvent::Auth(AuthEvent::Polled {
            attempt_id,
            poll_generation,
            response: status(AuthAttemptLifecycle::Failed, target("github")),
        });
        assert!(client.apply(terminal.clone()).effects().is_empty());
        assert!(client.apply(terminal).effects().is_empty());
        assert_eq!(
            client
                .state()
                .auth
                .active_attempt
                .as_ref()
                .unwrap()
                .lifecycle,
            AuthAttemptLifecycle::Failed as i32
        );
    }

    #[test]
    fn successful_callback_updates_target_status_and_keeps_terminal_snapshot() {
        let mut client = Client::new();
        let (attempt_id, poll_generation) = begin_callback(&mut client);
        let mut response = status(AuthAttemptLifecycle::Succeeded, target("github"));
        response.status = Some(crate::AuthStatus {
            target: Some(target("github")),
            kind: waltr::grpc::waltr::auth::v1::AuthStatusKind::OauthConfigured as i32,
            ..crate::AuthStatus::default()
        });

        assert!(
            client
                .apply(ClientEvent::Auth(AuthEvent::Polled {
                    attempt_id,
                    poll_generation,
                    response,
                }))
                .effects()
                .is_empty()
        );
        assert!(client.state().auth.statuses.contains_key("mcp:github"));
        assert_eq!(
            client
                .state()
                .auth
                .active_attempt
                .as_ref()
                .unwrap()
                .lifecycle,
            AuthAttemptLifecycle::Succeeded as i32
        );
    }

    #[test]
    fn stale_or_foreign_poll_results_cannot_mutate_active_attempt() {
        let mut client = Client::new();
        let (attempt_id, poll_generation) = begin_callback(&mut client);
        client.apply(ClientEvent::Auth(AuthEvent::Polled {
            attempt_id: attempt_id.clone(),
            poll_generation: poll_generation + 1,
            response: status(AuthAttemptLifecycle::Succeeded, target("github")),
        }));
        client.apply(ClientEvent::Auth(AuthEvent::Polled {
            attempt_id,
            poll_generation,
            response: status(AuthAttemptLifecycle::Succeeded, target("other")),
        }));
        let attempt = client.state().auth.active_attempt.as_ref().unwrap();
        assert_eq!(attempt.lifecycle, AuthAttemptLifecycle::Pending as i32);
        assert_eq!(attempt.poll_failures, 1);
    }

    #[test]
    fn polling_stops_after_bounded_transport_failures() {
        let mut client = Client::new();
        let (attempt_id, poll_generation) = begin_callback(&mut client);
        for failure in 1..=5 {
            let update = client.apply(ClientEvent::Auth(AuthEvent::PollFailed {
                attempt_id: attempt_id.clone(),
                poll_generation,
                error: crate::ClientError::new("raw transport body"),
            }));
            assert_eq!(update.effects().is_empty(), failure == 5);
        }
        let attempt = client.state().auth.active_attempt.as_ref().unwrap();
        assert_eq!(attempt.lifecycle, AuthAttemptLifecycle::Failed as i32);
        assert!(!attempt.failure_reason.contains("raw transport body"));
    }

    #[test]
    fn access_invalidation_clears_attempt_and_makes_late_poll_stale() {
        let mut client = Client::new();
        let (attempt_id, poll_generation) = begin_callback(&mut client);
        client.expire_waltr_access("access expired");
        assert!(client.state().auth.active_attempt.is_none());
        assert!(
            client
                .apply(ClientEvent::Auth(AuthEvent::Polled {
                    attempt_id,
                    poll_generation,
                    response: status(AuthAttemptLifecycle::Succeeded, target("github")),
                }))
                .effects()
                .is_empty()
        );
        assert!(client.state().auth.statuses.is_empty());
    }

    #[test]
    fn concurrent_target_refreshes_preserve_each_keyed_result() {
        let mut client = Client::new();
        let provider = AuthTarget {
            target: Some(auth_target::Target::ProviderKey("provider".to_owned())),
        };
        let mcp = target("github");
        let first = client.refresh_auth(provider.clone());
        let [
            ClientEffect::Auth(AuthEffect::Refresh {
                operation_key: first_key,
                ..
            }),
        ] = first.effects()
        else {
            panic!("first refresh effect");
        };
        let first_key = first_key.clone();
        let second = client.refresh_auth(mcp.clone());
        let [
            ClientEffect::Auth(AuthEffect::Refresh {
                operation_key: second_key,
                ..
            }),
        ] = second.effects()
        else {
            panic!("second refresh effect");
        };
        let second_key = second_key.clone();

        client.apply(ClientEvent::Auth(AuthEvent::Refreshed {
            operation_key: first_key.clone(),
            status: crate::AuthStatus {
                target: Some(provider),
                can_refresh: true,
                ..crate::AuthStatus::default()
            },
        }));
        client.apply(ClientEvent::Auth(AuthEvent::Refreshed {
            operation_key: second_key.clone(),
            status: crate::AuthStatus {
                target: Some(mcp),
                can_refresh: true,
                ..crate::AuthStatus::default()
            },
        }));

        assert!(matches!(
            client.state().operations.get(&first_key),
            Some(crate::ClientOperationState::AuthRefresh {
                status: crate::OperationStatus::Ready,
                ..
            })
        ));
        assert!(matches!(
            client.state().operations.get(&second_key),
            Some(crate::ClientOperationState::AuthRefresh {
                status: crate::OperationStatus::Ready,
                ..
            })
        ));
        assert_eq!(client.state().auth.statuses.len(), 2);
    }
}
