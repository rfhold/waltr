//! Session reducer responsibilities.

use crate::transport::{SessionWatchEvent, SessionWatchUpdate};
use crate::{
    ClientEffect, ClientState, ClientUpdate, OperationStatus, SessionEffect, SessionEvent,
    TimelineWatchKey,
};

#[allow(clippy::too_many_lines)]
#[allow(clippy::match_same_arms)]
pub fn reduce(state: &mut ClientState, event: SessionEvent) -> ClientUpdate {
    match event {
        SessionEvent::AgentsLoadRequested => maintain_agents(state, true),
        SessionEvent::AgentsLoaded {
            request_id,
            agents,
            default_agent_name,
        } => {
            state.set_session_agents_ready(request_id, agents, default_agent_name);
            ClientUpdate::default()
        }
        SessionEvent::AgentsLoadFailed { request_id, error } => {
            state.set_session_agents_failed(request_id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::CreateRequested {
            operation_key,
            agent_name,
        } => {
            state.set_session_create_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Create {
                operation_key,
                agent_name,
            }))
        }
        SessionEvent::Created {
            operation_key,
            session,
        } => {
            state.set_session_create_ready(operation_key, session);
            ClientUpdate::default()
        }
        SessionEvent::CreateFailed {
            operation_key,
            error,
        } => {
            state.set_session_create_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::DeleteRequested { operation_key, id } => {
            state.set_session_delete_loading(operation_key.clone(), id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Delete {
                operation_key,
                id,
            }))
        }
        SessionEvent::Deleted { operation_key, id } => {
            state.set_session_delete_ready(operation_key, id);
            ClientUpdate::default()
        }
        SessionEvent::DeleteFailed {
            operation_key,
            id,
            error,
        } => {
            state.set_session_delete_failed(operation_key, id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::WipeRequested(operation_key) => {
            state.set_session_wipe_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Wipe { operation_key }))
        }
        SessionEvent::Wiped { operation_key } => {
            state.set_session_wipe_ready(operation_key);
            ClientUpdate::default()
        }
        SessionEvent::WipeFailed {
            operation_key,
            error,
        } => {
            state.set_session_wipe_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::InferenceSettingsUpdateRequested {
            operation_key,
            id,
            inference_settings,
        } => {
            state.set_session_inference_settings_update_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Session(
                SessionEffect::UpdateInferenceSettings {
                    operation_key,
                    id,
                    inference_settings,
                },
            ))
        }
        SessionEvent::InferenceSettingsUpdated {
            operation_key,
            session,
        } => {
            state.set_session_inference_settings_update_ready(operation_key, session);
            ClientUpdate::default()
        }
        SessionEvent::InferenceSettingsUpdateFailed {
            operation_key,
            error,
        } => {
            state.set_session_inference_settings_update_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::SessionAgentUpdateRequested {
            operation_key,
            id,
            agent_name,
        } => {
            state.set_session_agent_update_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::UpdateAgent {
                operation_key,
                id,
                agent_name,
            }))
        }
        SessionEvent::SessionAgentUpdated {
            operation_key,
            session,
        } => {
            state.set_session_agent_update_ready(operation_key, session);
            ClientUpdate::default()
        }
        SessionEvent::SessionAgentUpdateFailed {
            operation_key,
            error,
        } => {
            state.set_session_agent_update_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::ListRequested => {
            state.set_session_list_loading();
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::List))
        }
        SessionEvent::Listed(sessions) => {
            state.set_session_list_ready(sessions);
            ClientUpdate::default()
        }
        SessionEvent::ListFailed(error) => {
            state.set_session_list_failed(error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::TimelineRequested { session_id, query } => {
            state.set_session_timeline_load_loading(&session_id);
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::GetTimeline {
                session_id,
                query,
            }))
        }
        SessionEvent::TimelineLoaded { session_id, events } => {
            state.set_session_timeline_load_ready(&session_id, events);
            ClientUpdate::default()
        }
        SessionEvent::TimelineLoadFailed { session_id, error } => {
            state.set_session_timeline_load_failed(&session_id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::TemplatesSearchRequested {
            request_id,
            session_id,
            query,
            limit,
            kinds,
            target,
            cursor,
        } => {
            state.set_session_template_search_loading(
                request_id.clone(),
                session_id.clone(),
                query.clone(),
            );
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::SearchTemplates {
                request_id,
                session_id,
                query,
                limit,
                kinds,
                target,
                cursor,
            }))
        }
        SessionEvent::TemplatesSearched {
            request_id,
            session_id,
            options,
        } => {
            state.set_session_template_search_ready(&request_id, &session_id, options);
            ClientUpdate::default()
        }
        SessionEvent::TemplatesSearchFailed {
            request_id,
            session_id,
            error,
        } => {
            state.set_session_template_search_failed(&request_id, &session_id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::WatchInterestRegistered => {
            state.add_session_watch_interest();
            let mut update = maintain_watch(state);
            update.extend(maintain_agents(state, false));
            update
        }
        SessionEvent::WatchInterestReleased => {
            state.remove_session_watch_interest();
            ClientUpdate::default()
        }
        SessionEvent::WatchStarted => {
            if let Some(open_id) = state.sessions.watch.open_id {
                state.set_session_watch_opened(open_id);
            }
            ClientUpdate::default()
        }
        SessionEvent::WatchStartedForOpen { open_id } => {
            state.set_session_watch_opened(open_id);
            ClientUpdate::default()
        }
        SessionEvent::WatchOpenFailed { open_id, error }
            if state.sessions.watch.open_id == Some(open_id) =>
        {
            watch_failed(state, error.to_string())
        }
        SessionEvent::WatchOpenFailed { .. } => ClientUpdate::default(),
        SessionEvent::WatchChanged(update) => apply_watch_update(state, update),
        SessionEvent::WatchFailed(error) => watch_failed(state, error.to_string()),
        SessionEvent::WatchEnded => watch_failed(state, "session watch ended".to_owned()),
        SessionEvent::ReconnectReady => maintain_watch(state),
        SessionEvent::TimelineWatchInterestRegistered {
            session_id,
            after_cursor,
            channels,
            initial_limit,
        } => {
            let key = TimelineWatchKey::new(session_id, after_cursor, channels, initial_limit);
            state.configure_session_timeline_watch(key.clone());
            maintain_timeline_watch(state, &key)
        }
        SessionEvent::TimelineWatchInterestReleased { session_id } => {
            state.remove_session_timeline_watch_interest(&session_id);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchStarted { key } => {
            if let Some(open_id) = state
                .sessions
                .timelines
                .get(&key.session_id)
                .and_then(|timeline| timeline.watches.get(&key))
                .and_then(|watch| watch.open_id)
            {
                state.set_session_timeline_watch_opened(&key, open_id);
            }
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchStartedForOpen { key, open_id } => {
            state.set_session_timeline_watch_opened(&key, open_id);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchOpenFailed {
            key,
            open_id,
            error,
        } if state
            .sessions
            .timelines
            .get(&key.session_id)
            .and_then(|timeline| timeline.watches.get(&key))
            .is_some_and(|watch| watch.open_id == Some(open_id)) =>
        {
            timeline_watch_failed(state, &key, error.to_string())
        }
        SessionEvent::TimelineWatchOpenFailed { .. } => ClientUpdate::default(),
        SessionEvent::TimelineWatchChanged { key, update } => {
            state.set_session_timeline_watch_changed(&key, update.sequence, update.event);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchFailed { key, error } => {
            timeline_watch_failed(state, &key, error.to_string())
        }
        SessionEvent::TimelineWatchEnded { key } => {
            timeline_watch_failed(state, &key, "timeline watch ended".to_owned())
        }
        SessionEvent::TimelineReconnectReady { key } => maintain_timeline_watch(state, &key),
        SessionEvent::UserMessageSubmitRequested {
            operation_key,
            session_id,
            text,
        } => {
            state
                .set_session_submit_user_message_loading(operation_key.clone(), session_id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::SubmitUserMessage {
                operation_key,
                session_id,
                text,
            }))
        }
        SessionEvent::UserMessageSubmitted { operation_key } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_submit_user_message_ready(operation_key, session_id);
            ClientUpdate::default()
        }
        SessionEvent::UserMessageSubmitFailed {
            operation_key,
            error,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_submit_user_message_failed(operation_key, session_id, &error);
            ClientUpdate::default()
        }
        SessionEvent::ExternalRequestRespondRequested {
            operation_key,
            session_id,
            request_id,
            response_json,
        } => {
            state.set_session_respond_external_request_loading(
                operation_key.clone(),
                session_id.clone(),
                request_id.clone(),
            );
            ClientUpdate::with_effect(ClientEffect::Session(
                SessionEffect::RespondExternalRequest {
                    operation_key,
                    session_id,
                    request_id,
                    response_json,
                },
            ))
        }
        SessionEvent::ExternalRequestResponded { operation_key } => {
            let (session_id, request_id) = operation_session_and_request_id(state, &operation_key);
            state.set_session_respond_external_request_ready(operation_key, session_id, request_id);
            ClientUpdate::default()
        }
        SessionEvent::ExternalRequestRespondFailed {
            operation_key,
            error,
        } => {
            let (session_id, request_id) = operation_session_and_request_id(state, &operation_key);
            state.set_session_respond_external_request_failed(
                operation_key,
                session_id,
                request_id,
                &error,
            );
            ClientUpdate::default()
        }
        SessionEvent::InterruptRequested {
            operation_key,
            session_id,
        } => {
            state.set_session_interrupt_loading(operation_key.clone(), session_id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Interrupt {
                operation_key,
                session_id,
            }))
        }
        SessionEvent::Interrupted { operation_key } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_interrupt_ready(operation_key, session_id);
            ClientUpdate::default()
        }
        SessionEvent::InterruptFailed {
            operation_key,
            error,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_interrupt_failed(operation_key, session_id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::RollbackRequested {
            operation_key,
            session_id,
            target,
        } => {
            state.set_session_rollback_loading(operation_key.clone(), session_id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Rollback {
                operation_key,
                session_id,
                target,
            }))
        }
        SessionEvent::RolledBack {
            operation_key,
            session,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_rollback_ready(operation_key, session_id, session);
            ClientUpdate::default()
        }
        SessionEvent::RollbackFailed {
            operation_key,
            error,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_rollback_failed(operation_key, session_id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::ContinueRequested {
            operation_key,
            session_id,
            target,
        } => {
            state.set_session_continue_loading(operation_key.clone(), session_id.clone(), target);
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Continue {
                operation_key,
                session_id,
                target,
            }))
        }
        SessionEvent::Continued { operation_key } => {
            let (session_id, target) = operation_session_and_target(state, &operation_key);
            state.set_session_continue_ready(operation_key, session_id, target);
            ClientUpdate::default()
        }
        SessionEvent::ContinueFailed {
            operation_key,
            error,
        } => {
            let (session_id, target) = operation_session_and_target(state, &operation_key);
            state.set_session_continue_failed(operation_key, session_id, target, &error);
            ClientUpdate::default()
        }
    }
}

pub fn maintain_agents(state: &mut ClientState, force: bool) -> ClientUpdate {
    if state.sessions.watch.interests == 0 || !state.permits_watch_maintenance() {
        return ClientUpdate::default();
    }
    if !force
        && matches!(
            state.sessions.agents.load.status,
            OperationStatus::Loading | OperationStatus::Ready
        )
    {
        return ClientUpdate::default();
    }
    let request_id = state.set_session_agents_loading();
    ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::ListAgents {
        request_id,
    }))
}

pub fn maintain_watch(state: &mut ClientState) -> ClientUpdate {
    if state.sessions.watch.interests == 0 {
        return ClientUpdate::default();
    }

    if matches!(
        state.sessions.watch.status,
        OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
    ) {
        return ClientUpdate::default();
    }

    if !state.permits_watch_maintenance() {
        state.set_session_watch_waiting();
        return ClientUpdate::default();
    }

    let Some(open_id) = state.set_session_watch_loading() else {
        return ClientUpdate::default();
    };
    ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Watch { open_id }))
}

pub fn maintain_timeline_watch(state: &mut ClientState, key: &TimelineWatchKey) -> ClientUpdate {
    if !state.session_timeline_watch_is_active(key) {
        return ClientUpdate::default();
    }

    let Some(timeline) = state.sessions.timelines.get(&key.session_id) else {
        return ClientUpdate::default();
    };

    let Some(watch) = timeline.watches.get(key) else {
        return ClientUpdate::default();
    };

    if matches!(
        watch.status,
        OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
    ) {
        return ClientUpdate::default();
    }

    if !state.permits_watch_maintenance() {
        state.set_session_timeline_watch_waiting(key);
        return ClientUpdate::default();
    }

    let Some((after_cursor, channels, initial_limit)) = state.session_timeline_watch_request(key)
    else {
        return ClientUpdate::default();
    };

    let Some(open_id) = state.set_session_timeline_watch_loading(key) else {
        return ClientUpdate::default();
    };
    ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::WatchTimeline {
        key: TimelineWatchKey::new(
            key.session_id.clone(),
            after_cursor,
            channels,
            initial_limit,
        ),
        open_id,
    }))
}

fn apply_watch_update(state: &mut ClientState, update: SessionWatchUpdate) -> ClientUpdate {
    if matches!(update.event, SessionWatchEvent::Snapshot(_)) {
        apply_watch_event(state, update);
        return ClientUpdate::default();
    }

    if update.sequence != state.sessions.last_sequence.saturating_add(1) {
        apply_watch_event(state, update);
        return ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::List));
    }

    apply_watch_event(state, update);
    ClientUpdate::default()
}

fn timeline_watch_failed(
    state: &mut ClientState,
    key: &TimelineWatchKey,
    message: String,
) -> ClientUpdate {
    if !state.session_timeline_watch_is_active(key) {
        return ClientUpdate::default();
    }

    let delay_ms = state.set_session_timeline_watch_failed(key, message);

    if state.permits_watch_maintenance() {
        ClientUpdate::with_effect(ClientEffect::Session(
            SessionEffect::SleepTimelineReconnect {
                key: key.clone(),
                delay_ms,
            },
        ))
    } else {
        state.set_session_timeline_watch_waiting(key);
        ClientUpdate::default()
    }
}

fn operation_session_id(state: &ClientState, operation_key: &str) -> String {
    match state.operations.get(operation_key) {
        Some(
            crate::ClientOperationState::SessionSubmitUserMessage { session_id, .. }
            | crate::ClientOperationState::SessionInterrupt { session_id, .. }
            | crate::ClientOperationState::SessionRollback { session_id, .. },
        ) => session_id.clone(),
        _ => String::new(),
    }
}

fn operation_session_and_target(
    state: &ClientState,
    operation_key: &str,
) -> (String, crate::RollbackTarget) {
    match state.operations.get(operation_key) {
        Some(crate::ClientOperationState::SessionContinue {
            session_id, target, ..
        }) => (session_id.clone(), *target),
        _ => (String::new(), crate::RollbackTarget { target: None }),
    }
}

fn operation_session_and_request_id(state: &ClientState, operation_key: &str) -> (String, String) {
    match state.operations.get(operation_key) {
        Some(crate::ClientOperationState::SessionRespondExternalRequest {
            session_id,
            request_id,
            ..
        }) => (session_id.clone(), request_id.clone()),
        _ => (String::new(), String::new()),
    }
}

#[allow(clippy::semicolon_if_nothing_returned)]
fn apply_watch_event(state: &mut ClientState, update: SessionWatchUpdate) {
    match update.event {
        SessionWatchEvent::Snapshot(sessions) => {
            state.set_session_watch_snapshot(update.sequence, sessions)
        }
        SessionWatchEvent::Upsert(session) => {
            state.set_session_watch_changed(update.sequence, session)
        }
        SessionWatchEvent::Deleted(id) => state.set_session_watch_deleted(update.sequence, &id),
        SessionWatchEvent::Cleared => state.set_session_watch_cleared(update.sequence),
    }
}

fn watch_failed(state: &mut ClientState, message: String) -> ClientUpdate {
    if state.sessions.watch.interests == 0 {
        return ClientUpdate::default();
    }

    let delay_ms = state.set_session_watch_failed(message);

    if state.permits_watch_maintenance() {
        ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::SleepReconnect {
            delay_ms,
        }))
    } else {
        state.set_session_watch_waiting();
        ClientUpdate::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Client, ClientError, ClientEvent, McpAuthorizationContext, OutboundAuthenticationFailure,
        SessionEffect,
    };
    use waltr::grpc::waltr::auth::v1::{
        AuthenticationDomain, AuthenticationFailureReason, BeginAuthResponse, auth_target,
    };
    use waltr::grpc::waltr::session::v1::SelectableAgent;

    #[test]
    fn stale_agent_catalog_completion_is_ignored_and_entries_are_sorted() {
        let mut client = Client::new();
        client.apply(ClientEvent::Status(crate::StatusEvent::WatchChanged(
            crate::SystemStatus { ready: true },
        )));
        let first = client.register_session_watch_interest();
        let request_id = first
            .effects()
            .iter()
            .find_map(|effect| match effect {
                ClientEffect::Session(SessionEffect::ListAgents { request_id }) => {
                    Some(*request_id)
                }
                _ => None,
            })
            .expect("agent catalog effect");
        let second = client.request_session_agents();
        let newer_id = match second.effects() {
            [ClientEffect::Session(SessionEffect::ListAgents { request_id })] => *request_id,
            effects => panic!("unexpected effects: {effects:?}"),
        };

        client.apply(ClientEvent::Session(SessionEvent::AgentsLoaded {
            request_id,
            agents: vec![SelectableAgent {
                name: "stale".to_owned(),
                description: None,
            }],
            default_agent_name: "stale".to_owned(),
        }));
        assert!(client.state().sessions.agents.entries.is_empty());

        client.apply(ClientEvent::Session(SessionEvent::AgentsLoaded {
            request_id: newer_id,
            agents: vec![
                SelectableAgent {
                    name: "zeta".to_owned(),
                    description: None,
                },
                SelectableAgent {
                    name: "alpha".to_owned(),
                    description: Some("Alpha".to_owned()),
                },
            ],
            default_agent_name: "alpha".to_owned(),
        }));
        assert_eq!(client.state().sessions.agents.entries[0].name, "alpha");
        assert_eq!(client.state().sessions.agents.default_agent_name, "alpha");
    }

    #[test]
    fn stale_agent_update_does_not_close_newer_aggregate_operation() {
        let mut client = Client::new();
        let first = client.update_session_agent("session-1", "alpha");
        let first_key = match first.effects() {
            [ClientEffect::Session(SessionEffect::UpdateAgent { operation_key, .. })] => {
                operation_key.clone()
            }
            effects => panic!("unexpected effects: {effects:?}"),
        };
        let second = client.update_session_agent("session-1", "beta");
        let second_key = match second.effects() {
            [ClientEffect::Session(SessionEffect::UpdateAgent { operation_key, .. })] => {
                operation_key.clone()
            }
            effects => panic!("unexpected effects: {effects:?}"),
        };

        client.apply(ClientEvent::Session(SessionEvent::SessionAgentUpdated {
            operation_key: first_key.clone(),
            session: crate::SessionSummary {
                id: "session-1".to_owned(),
                agent_name: "alpha".to_owned(),
                ..crate::SessionSummary::default()
            },
        }));
        assert_eq!(client.state().sessions.agent_update.key, Some(second_key));
        assert_eq!(
            client.state().sessions.agent_update.status,
            OperationStatus::Loading
        );
        assert!(client.state().sessions.current.is_empty());
        assert!(matches!(
            client.state().operations.get(&first_key),
            Some(crate::ClientOperationState::SessionAgentUpdate {
                status: OperationStatus::Ready,
                ..
            })
        ));
    }

    fn target() -> crate::AuthTarget {
        crate::AuthTarget {
            target: Some(auth_target::Target::McpServerKey("github".to_owned())),
        }
    }

    fn auth_error() -> ClientError {
        ClientError::outbound_authentication(
            "unauthenticated: authorize MCP",
            OutboundAuthenticationFailure {
                version: 1,
                domain: AuthenticationDomain::OutboundMcp as i32,
                reason: AuthenticationFailureReason::NetworkFailed as i32,
                target: target(),
                mcp_context: Some(McpAuthorizationContext {
                    endpoint: Some("https://mcp.example/rpc".to_owned()),
                    http_status: 401,
                    scope: Some("repo:read".to_owned()),
                    ..McpAuthorizationContext::default()
                }),
            },
        )
    }

    fn seed_active_attempt(client: &mut Client) {
        let update = client.begin_auth(target());
        let [ClientEffect::Auth(crate::AuthEffect::Begin { operation_key, .. })] = update.effects()
        else {
            panic!("begin auth effect");
        };
        client.apply(ClientEvent::Auth(crate::AuthEvent::Begun {
            operation_key: operation_key.clone(),
            target: target(),
            attempt: BeginAuthResponse {
                attempt_id: "attempt-1".to_owned(),
                auth_url: "https://auth.example/authorize".to_owned(),
                ..BeginAuthResponse::default()
            },
        }));
    }

    fn assert_failed_operation(
        client: &Client,
        session_id: &str,
        active_attempt: Option<&crate::AuthAttemptState>,
    ) {
        let operation = client
            .state()
            .operations
            .values()
            .find(|operation| {
                matches!(
                    operation,
                    crate::ClientOperationState::SessionSubmitUserMessage { .. }
                        | crate::ClientOperationState::SessionRespondExternalRequest { .. }
                        | crate::ClientOperationState::SessionContinue { .. }
                )
            })
            .expect("keyed operation");
        let (actual_session_id, failure) = match operation {
            crate::ClientOperationState::SessionSubmitUserMessage {
                session_id,
                authentication_failure,
                ..
            }
            | crate::ClientOperationState::SessionRespondExternalRequest {
                session_id,
                authentication_failure,
                ..
            }
            | crate::ClientOperationState::SessionContinue {
                session_id,
                authentication_failure,
                ..
            } => (session_id, authentication_failure.as_ref().unwrap()),
            _ => panic!("session run operation"),
        };
        assert_eq!(actual_session_id, session_id);
        assert_eq!(failure.domain, AuthenticationDomain::OutboundMcp as i32);
        assert_eq!(
            failure.reason,
            AuthenticationFailureReason::NetworkFailed as i32
        );
        assert_eq!(failure.target, target());
        assert_eq!(
            failure
                .mcp_context
                .as_ref()
                .and_then(|value| value.scope.as_deref()),
            Some("repo:read")
        );
        assert_eq!(client.state().auth.active_attempt.as_ref(), active_attempt);

        let exposed = match crate::ClientOperationView::from(operation.clone()) {
            crate::ClientOperationView::SessionSubmitUserMessage {
                authentication_failure,
                ..
            }
            | crate::ClientOperationView::SessionRespondExternalRequest {
                authentication_failure,
                ..
            }
            | crate::ClientOperationView::SessionContinue {
                authentication_failure,
                ..
            } => authentication_failure.unwrap(),
            _ => panic!("session run operation view"),
        };
        assert_eq!(exposed.domain, "outboundMcp");
        assert_eq!(exposed.reason, "networkFailed");
        assert_eq!(exposed.target.key, "github");
    }

    #[test]
    fn failed_submit_retains_outbound_auth_without_auth_effects() {
        let mut client = Client::new();
        seed_active_attempt(&mut client);
        let active_attempt = client.state().auth.active_attempt.clone();
        let update = client.submit_user_message("submit-session", "hello");
        let [ClientEffect::Session(SessionEffect::SubmitUserMessage { operation_key, .. })] =
            update.effects()
        else {
            panic!("submit effect")
        };
        let update = client.apply(ClientEvent::Session(
            SessionEvent::UserMessageSubmitFailed {
                operation_key: operation_key.clone(),
                error: auth_error(),
            },
        ));
        assert!(update.effects().is_empty());
        assert_failed_operation(&client, "submit-session", active_attempt.as_ref());
    }

    #[test]
    fn failed_response_retains_outbound_auth_without_auth_effects() {
        let mut client = Client::new();
        seed_active_attempt(&mut client);
        let active_attempt = client.state().auth.active_attempt.clone();
        let update = client.respond_external_request("respond-session", "request", "{}");
        let [ClientEffect::Session(SessionEffect::RespondExternalRequest { operation_key, .. })] =
            update.effects()
        else {
            panic!("respond effect")
        };
        let update = client.apply(ClientEvent::Session(
            SessionEvent::ExternalRequestRespondFailed {
                operation_key: operation_key.clone(),
                error: auth_error(),
            },
        ));
        assert!(update.effects().is_empty());
        assert_failed_operation(&client, "respond-session", active_attempt.as_ref());
    }

    #[test]
    fn failed_continue_retains_outbound_auth_without_auth_effects() {
        let mut client = Client::new();
        seed_active_attempt(&mut client);
        let active_attempt = client.state().auth.active_attempt.clone();
        let update = client.continue_session_from_turn("continue-session", 1);
        let [ClientEffect::Session(SessionEffect::Continue { operation_key, .. })] =
            update.effects()
        else {
            panic!("continue effect")
        };
        let update = client.apply(ClientEvent::Session(SessionEvent::ContinueFailed {
            operation_key: operation_key.clone(),
            error: auth_error(),
        }));
        assert!(update.effects().is_empty());
        assert_failed_operation(&client, "continue-session", active_attempt.as_ref());
    }
}
