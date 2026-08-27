//! Shared async executor for client side-effect intents.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;

use crate::transport::{
    ConfigWatchUpdate, DefaultGrpcTransport, WaltrTransport, RoutineDefinitionWatchStream,
    RoutineInvocationWatchStream, SessionTimelineWatchStream, SessionWatchStream,
    StatusWatchStream,
};
use crate::{
    AuthEffect, AuthEvent, CatalogEffect, CatalogEvent, ClientEffect, ClientError, ClientEvent,
    ConfigEffect, ConfigEvent, ConfigView, ResourceCatalog, ResourceWrite, RoutineEffect,
    RoutineEvent, SearchCatalogModelsRequest, SearchSessionTemplatesRequest, SessionEffect,
    SessionEvent, SessionSummary, StartManualRoutineArgs, StatusEffect, StatusEvent,
    SuggestCatalogMatchesRequest, search_session_templates_response,
};

/// Stream of configuration changes observed through the gRPC executor.
pub type ConfigChangeStream = Pin<Box<dyn Stream<Item = Result<ConfigChange, ClientError>>>>;

pub type ConfigWatchUpdateStream =
    Pin<Box<dyn Stream<Item = Result<ConfigWatchUpdate, ClientError>>>>;

/// Stream of system readiness changes observed through the gRPC executor.
pub type StatusChangeStream = StatusWatchStream;

/// Stream of session changes observed through the gRPC executor.
pub type SessionChangeStream = SessionWatchStream;

/// Stream of session watch updates observed through the gRPC executor.
pub type SessionWatchUpdateStream = SessionWatchStream;

/// Stream of timeline watch updates observed through the gRPC executor.
pub type SessionTimelineWatchUpdateStream = SessionTimelineWatchStream;

/// Stable shell-facing configuration watch change.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChange {
    /// Previous configuration version.
    pub previous_etag: String,
    /// New configuration version.
    pub etag: String,
    /// Configuration keys changed by the update.
    pub changed_keys: Vec<String>,
    /// Server-reported watch error, if any.
    pub error: String,
    /// New configuration snapshot when provided by the server.
    pub config: Option<ConfigView>,
}

/// Shared gRPC-backed executor for client side-effect intents.
#[derive(Clone, Debug)]
pub struct GrpcEffectExecutor {
    transport: DefaultGrpcTransport,
}

impl GrpcEffectExecutor {
    /// Create an executor from a runtime-owned default transport.
    #[must_use]
    pub const fn with_transport(transport: DefaultGrpcTransport) -> Self {
        Self { transport }
    }

    /// Create a gRPC effect executor for an endpoint base URL.
    #[must_use]
    pub fn for_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            transport: DefaultGrpcTransport::new(endpoint),
        }
    }

    /// Return the underlying transport.
    #[must_use]
    pub const fn transport(&self) -> &DefaultGrpcTransport {
        &self.transport
    }

    /// Load the current configuration as the stable shell-facing view model.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot load configuration.
    pub async fn load_config(&self) -> Result<ConfigView, ClientError> {
        self.transport
            .get_config()
            .await
            .and_then(catalog_config_view)
    }

    /// Load the authoritative resource catalog used as a write baseline.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot load the catalog.
    pub async fn load_resource_catalog(&self) -> Result<ResourceCatalog, ClientError> {
        self.transport.get_config().await
    }

    /// Apply one prepared canonical resource-slot write.
    ///
    /// # Errors
    ///
    /// Returns an error when replacement or reset fails.
    pub async fn write_resource(&self, write: ResourceWrite) -> Result<(), ClientError> {
        self.transport.write_resource(write).await
    }

    /// Subscribe to configuration changes as stable shell-facing view models.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot open the config watch stream.
    pub async fn watch_config(&self) -> Result<ConfigChangeStream, ClientError> {
        let stream = self
            .transport
            .watch_config()
            .await?
            .map(|event| event.map(ConfigChange::from));

        Ok(Box::pin(stream))
    }

    pub(crate) async fn watch_config_updates(
        &self,
    ) -> Result<ConfigWatchUpdateStream, ClientError> {
        let stream = self.transport.watch_config().await?;

        Ok(Box::pin(stream))
    }

    /// Subscribe to system readiness changes.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot open the status watch stream.
    pub async fn watch_status(&self) -> Result<StatusChangeStream, ClientError> {
        self.transport.watch_status().await
    }

    /// Create a new server-side session.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot create the session.
    pub async fn create_session(&self, agent_name: String) -> Result<SessionSummary, ClientError> {
        self.transport.create_session(agent_name).await
    }

    /// List current server-side sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot list sessions.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        self.transport.list_sessions().await
    }

    /// Delete a server-side session by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot delete the session.
    pub async fn delete_session(&self, id: impl Into<String>) -> Result<(), ClientError> {
        self.transport.delete_session(id.into()).await
    }

    /// Delete every server-side session.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot wipe sessions.
    pub async fn wipe_sessions(&self) -> Result<(), ClientError> {
        self.transport.wipe_sessions().await
    }

    /// Subscribe to session changes.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot open the session watch stream.
    pub async fn watch_sessions(&self) -> Result<SessionChangeStream, ClientError> {
        self.transport.watch_sessions().await
    }

    pub(crate) async fn watch_session_updates(
        &self,
    ) -> Result<SessionWatchUpdateStream, ClientError> {
        self.transport.watch_sessions().await
    }

    pub(crate) async fn watch_session_timeline_updates(
        &self,
        session_id: String,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
        projection_mode: crate::SessionTimelineProjectionMode,
    ) -> Result<SessionTimelineWatchUpdateStream, ClientError> {
        self.transport
            .watch_timeline(
                session_id,
                after_cursor,
                channels,
                initial_limit,
                projection_mode,
            )
            .await
    }

    pub(crate) async fn watch_routine_definition_updates(
        &self,
        request_id: String,
    ) -> Result<RoutineDefinitionWatchStream, ClientError> {
        self.transport.watch_manual_routines(request_id).await
    }

    pub(crate) async fn watch_routine_invocation_updates(
        &self,
        request_id: String,
        after_sequence: Option<u64>,
        metadata_filters: std::collections::BTreeMap<String, String>,
    ) -> Result<RoutineInvocationWatchStream, ClientError> {
        self.transport
            .watch_routine_invocations(
                request_id,
                after_sequence,
                crate::ROUTINE_INVOCATION_SNAPSHOT_SIZE,
                metadata_filters,
            )
            .await
    }

    /// Execute one client side-effect intent and return client events for the reducer.
    #[allow(
        clippy::future_not_send,
        clippy::too_many_lines,
        reason = "client shell runtimes execute effects on a local task set"
    )]
    pub async fn execute_effect<Sleep, SleepFuture>(
        &self,
        effect: ClientEffect,
        sleep: Sleep,
    ) -> Vec<ClientEvent>
    where
        Sleep: FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        self.execute_effect_boxed(effect, sleep).await
    }

    #[allow(
        clippy::large_stack_frames,
        clippy::too_many_lines,
        reason = "the large effect future is boxed before it is polled"
    )]
    fn execute_effect_boxed<'a, Sleep, SleepFuture>(
        &'a self,
        effect: ClientEffect,
        sleep: Sleep,
    ) -> Pin<Box<dyn Future<Output = Vec<ClientEvent>> + 'a>>
    where
        Sleep: FnOnce(u64) -> SleepFuture + 'a,
        SleepFuture: Future<Output = ()> + 'a,
    {
        Box::pin(async move {
            match effect {
                ClientEffect::Auth(AuthEffect::ListStatuses) => {
                    match self.transport.list_auth_statuses().await {
                        Ok(statuses) => vec![ClientEvent::Auth(AuthEvent::Listed(statuses))],
                        Err(error) => vec![ClientEvent::Auth(AuthEvent::ListFailed(error))],
                    }
                }
                ClientEffect::Auth(AuthEffect::Begin {
                    operation_key,
                    target,
                }) => match self.transport.begin_auth(target.clone()).await {
                    Ok(attempt) => vec![ClientEvent::Auth(AuthEvent::Begun {
                        operation_key,
                        target,
                        attempt,
                    })],
                    Err(error) => vec![ClientEvent::Auth(AuthEvent::BeginFailed {
                        operation_key,
                        target,
                        error,
                    })],
                },
                ClientEffect::Auth(AuthEffect::Complete {
                    operation_key,
                    attempt_id,
                    input,
                }) => match self.transport.complete_auth(attempt_id, input).await {
                    Ok(status) => vec![ClientEvent::Auth(AuthEvent::Completed {
                        operation_key,
                        status,
                    })],
                    Err(error) => vec![ClientEvent::Auth(AuthEvent::CompleteFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Auth(AuthEffect::Poll {
                    attempt_id,
                    poll_generation,
                    delay_ms,
                }) => {
                    sleep(delay_ms).await;
                    match self
                        .transport
                        .get_auth_attempt_status(attempt_id.clone())
                        .await
                    {
                        Ok(response) => vec![ClientEvent::Auth(AuthEvent::Polled {
                            attempt_id,
                            poll_generation,
                            response,
                        })],
                        Err(error) => vec![ClientEvent::Auth(AuthEvent::PollFailed {
                            attempt_id,
                            poll_generation,
                            error,
                        })],
                    }
                }
                ClientEffect::Auth(AuthEffect::Cancel {
                    operation_key,
                    attempt_id,
                }) => match self.transport.cancel_auth(attempt_id).await {
                    Ok(()) => vec![ClientEvent::Auth(AuthEvent::Canceled { operation_key })],
                    Err(error) => vec![ClientEvent::Auth(AuthEvent::CancelFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Auth(AuthEffect::Refresh {
                    operation_key,
                    target,
                }) => match self.transport.refresh_auth(target).await {
                    Ok(status) => vec![ClientEvent::Auth(AuthEvent::Refreshed {
                        operation_key,
                        status,
                    })],
                    Err(error) => vec![ClientEvent::Auth(AuthEvent::RefreshFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Auth(AuthEffect::Logout {
                    operation_key,
                    target,
                }) => match self.transport.logout(target).await {
                    Ok(status) => vec![ClientEvent::Auth(AuthEvent::LoggedOut {
                        operation_key,
                        status,
                    })],
                    Err(error) => vec![ClientEvent::Auth(AuthEvent::LogoutFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Catalog(CatalogEffect::SearchModels {
                    request_id,
                    query,
                    provider_id,
                    limit,
                }) => match self
                    .transport
                    .search_catalog_models(SearchCatalogModelsRequest {
                        query,
                        provider_id: provider_id.unwrap_or_default(),
                        limit,
                    })
                    .await
                {
                    Ok(response) => vec![ClientEvent::Catalog(CatalogEvent::ModelsSearched {
                        request_id,
                        models: response.models,
                        stale: response.stale,
                    })],
                    Err(error) => vec![ClientEvent::Catalog(CatalogEvent::ModelsSearchFailed {
                        request_id,
                        error,
                    })],
                },
                ClientEffect::Catalog(CatalogEffect::SuggestMatches {
                    request_id,
                    provider_key,
                    model_key,
                    limit,
                }) => match self
                    .transport
                    .suggest_catalog_matches(SuggestCatalogMatchesRequest {
                        provider_key,
                        model_key,
                        limit,
                    })
                    .await
                {
                    Ok(response) => vec![ClientEvent::Catalog(CatalogEvent::MatchesSuggested {
                        request_id,
                        matches: response.matches,
                        stale: response.stale,
                    })],
                    Err(error) => vec![ClientEvent::Catalog(CatalogEvent::MatchesSuggestFailed {
                        request_id,
                        error,
                    })],
                },
                ClientEffect::Routine(RoutineEffect::StartManual {
                    operation_key,
                    request_id,
                    routine_name,
                    trigger_name,
                    source_session_id,
                    inputs,
                }) => match self
                    .transport
                    .start_manual_routine(StartManualRoutineArgs {
                        request_id: request_id.clone(),
                        routine_name,
                        trigger_name,
                        source_session_id,
                        inputs,
                    })
                    .await
                {
                    Ok(crate::ClientRoutineStartOutcome::Accepted(invocation_id)) => {
                        vec![ClientEvent::Routine(RoutineEvent::StartAccepted {
                            operation_key,
                            request_id,
                            invocation_id,
                        })]
                    }
                    Ok(crate::ClientRoutineStartOutcome::ValidationFailed(failure)) => {
                        vec![ClientEvent::Routine(RoutineEvent::StartValidationFailed {
                            operation_key,
                            request_id,
                            failure,
                        })]
                    }
                    Err(error) => vec![ClientEvent::Routine(RoutineEvent::StartFailed {
                        operation_key,
                        request_id,
                        error,
                    })],
                },
                ClientEffect::Routine(RoutineEffect::ListInvocations {
                    request_id,
                    page_size,
                    before_admission_sequence,
                    metadata_filters,
                }) => match self
                    .transport
                    .list_routine_invocations(
                        request_id.clone(),
                        page_size,
                        Some(before_admission_sequence),
                        metadata_filters,
                    )
                    .await
                {
                    Ok(page) => vec![ClientEvent::Routine(RoutineEvent::OlderPageLoaded {
                        request_id,
                        before_admission_sequence,
                        page,
                    })],
                    Err(error) => vec![ClientEvent::Routine(RoutineEvent::OlderPageFailed {
                        request_id,
                        before_admission_sequence,
                        error,
                    })],
                },
                ClientEffect::Routine(RoutineEffect::GetInvocation {
                    request_id,
                    invocation_id,
                }) => match self
                    .transport
                    .get_routine_invocation(request_id.clone(), invocation_id.clone())
                    .await
                {
                    Ok(detail) => vec![ClientEvent::Routine(RoutineEvent::DetailLoaded {
                        request_id,
                        invocation_id,
                        detail,
                    })],
                    Err(error) => vec![ClientEvent::Routine(RoutineEvent::DetailFailed {
                        request_id,
                        invocation_id,
                        error,
                    })],
                },
                ClientEffect::Config(ConfigEffect::Load) => match self.transport.get_config().await
                {
                    Ok(config) => vec![ClientEvent::Config(ConfigEvent::Loaded(config))],
                    Err(error) => vec![ClientEvent::Config(ConfigEvent::LoadFailed(error))],
                },
                ClientEffect::Config(ConfigEffect::Patch {
                    operation_key,
                    writes,
                }) => match write_resources(&self.transport, writes).await {
                    Ok(()) => match self.transport.get_config().await {
                        Ok(catalog) => vec![
                            ClientEvent::Config(ConfigEvent::Loaded(catalog)),
                            ClientEvent::Config(ConfigEvent::Patched { operation_key }),
                        ],
                        Err(error) => vec![ClientEvent::Config(ConfigEvent::PatchFailed {
                            operation_key,
                            error,
                        })],
                    },
                    Err(error) => match self.transport.get_config().await {
                        Ok(catalog) => vec![
                            ClientEvent::Config(ConfigEvent::Loaded(catalog)),
                            ClientEvent::Config(ConfigEvent::PatchFailed {
                                operation_key,
                                error,
                            }),
                        ],
                        Err(reload_error) => {
                            vec![ClientEvent::Config(ConfigEvent::PatchFailed {
                                operation_key,
                                error: reload_failure(&error, &reload_error),
                            })]
                        }
                    },
                },
                ClientEffect::Config(ConfigEffect::Reset {
                    operation_key,
                    writes,
                }) => match write_resources(&self.transport, writes).await {
                    Ok(()) => match self.transport.get_config().await {
                        Ok(catalog) => vec![
                            ClientEvent::Config(ConfigEvent::Loaded(catalog)),
                            ClientEvent::Config(ConfigEvent::Reset { operation_key }),
                        ],
                        Err(error) => vec![ClientEvent::Config(ConfigEvent::ResetFailed {
                            operation_key,
                            error,
                        })],
                    },
                    Err(error) => match self.transport.get_config().await {
                        Ok(catalog) => vec![
                            ClientEvent::Config(ConfigEvent::Loaded(catalog)),
                            ClientEvent::Config(ConfigEvent::ResetFailed {
                                operation_key,
                                error,
                            }),
                        ],
                        Err(reload_error) => {
                            vec![ClientEvent::Config(ConfigEvent::ResetFailed {
                                operation_key,
                                error: reload_failure(&error, &reload_error),
                            })]
                        }
                    },
                },
                ClientEffect::Config(ConfigEffect::Watch { open_id }) => {
                    match self.transport.watch_config().await {
                        Ok(_) => vec![ClientEvent::Config(ConfigEvent::WatchStartedForOpen {
                            open_id,
                        })],
                        Err(error) => vec![ClientEvent::Config(ConfigEvent::WatchOpenFailed {
                            open_id,
                            error,
                        })],
                    }
                }
                ClientEffect::Status(StatusEffect::Watch { epoch }) => {
                    match self.transport.watch_status().await {
                        Ok(_) => vec![ClientEvent::Status(StatusEvent::WatchStartedForEpoch {
                            epoch,
                        })],
                        Err(error) => vec![ClientEvent::Status(StatusEvent::WatchFailedForEpoch {
                            epoch,
                            error,
                        })],
                    }
                }
                ClientEffect::Session(SessionEffect::Create {
                    operation_key,
                    agent_name,
                }) => match self.transport.create_session(agent_name).await {
                    Ok(session) => vec![ClientEvent::Session(SessionEvent::Created {
                        operation_key,
                        session,
                    })],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::CreateFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Session(SessionEffect::Delete { operation_key, id }) => {
                    match self.transport.delete_session(id.clone()).await {
                        Ok(()) => vec![ClientEvent::Session(SessionEvent::Deleted {
                            operation_key,
                            id,
                        })],
                        Err(error) => vec![ClientEvent::Session(SessionEvent::DeleteFailed {
                            operation_key,
                            id,
                            error,
                        })],
                    }
                }
                ClientEffect::Session(SessionEffect::Wipe { operation_key }) => {
                    match self.transport.wipe_sessions().await {
                        Ok(()) => vec![ClientEvent::Session(SessionEvent::Wiped { operation_key })],
                        Err(error) => vec![ClientEvent::Session(SessionEvent::WipeFailed {
                            operation_key,
                            error,
                        })],
                    }
                }
                ClientEffect::Session(SessionEffect::UpdateInferenceSettings {
                    operation_key,
                    id,
                    inference_settings,
                }) => {
                    match self
                        .transport
                        .update_session_inference_settings(id, *inference_settings)
                        .await
                    {
                        Ok(session) => vec![ClientEvent::Session(
                            SessionEvent::InferenceSettingsUpdated {
                                operation_key,
                                session,
                            },
                        )],
                        Err(error) => vec![ClientEvent::Session(
                            SessionEvent::InferenceSettingsUpdateFailed {
                                operation_key,
                                error,
                            },
                        )],
                    }
                }
                ClientEffect::Session(SessionEffect::UpdateAgent {
                    operation_key,
                    id,
                    agent_name,
                }) => match self.transport.update_session_agent(id, agent_name).await {
                    Ok(session) => vec![ClientEvent::Session(SessionEvent::SessionAgentUpdated {
                        operation_key,
                        session,
                    })],
                    Err(error) => vec![ClientEvent::Session(
                        SessionEvent::SessionAgentUpdateFailed {
                            operation_key,
                            error,
                        },
                    )],
                },
                ClientEffect::Session(SessionEffect::ListAgents { request_id }) => {
                    match self.transport.list_agents().await {
                        Ok(response) => vec![ClientEvent::Session(SessionEvent::AgentsLoaded {
                            request_id,
                            agents: response.agents,
                            default_agent_name: response.default_agent_name,
                        })],
                        Err(error) => vec![ClientEvent::Session(SessionEvent::AgentsLoadFailed {
                            request_id,
                            error,
                        })],
                    }
                }
                ClientEffect::Session(SessionEffect::List) => {
                    match self.transport.list_sessions().await {
                        Ok(sessions) => vec![ClientEvent::Session(SessionEvent::Listed(sessions))],
                        Err(error) => vec![ClientEvent::Session(SessionEvent::ListFailed(error))],
                    }
                }
                ClientEffect::Session(SessionEffect::GetTimeline {
                    session_id,
                    request_id,
                    baseline_mode,
                    baseline_revision,
                    query,
                    projection_mode,
                }) => {
                    match self
                        .transport
                        .get_timeline(session_id.clone(), query, projection_mode)
                        .await
                    {
                        Ok(page) => vec![ClientEvent::Session(SessionEvent::TimelineLoaded {
                            session_id,
                            request_id,
                            baseline_mode,
                            baseline_revision,
                            page,
                        })],
                        Err(error) => {
                            vec![ClientEvent::Session(SessionEvent::TimelineLoadFailed {
                                session_id,
                                request_id,
                                baseline_mode,
                                baseline_revision,
                                error,
                            })]
                        }
                    }
                }
                ClientEffect::Session(SessionEffect::SearchTemplates {
                    request_id,
                    session_id,
                    query,
                    limit,
                    kinds,
                    target,
                    cursor,
                }) => match self
                    .transport
                    .search_session_templates(SearchSessionTemplatesRequest {
                        request_id: request_id.clone(),
                        session_id: session_id.clone(),
                        query,
                        limit,
                        kinds: kinds.into_iter().map(|kind| kind as i32).collect(),
                        target: target as i32,
                        cursor,
                    })
                    .await
                {
                    Ok(response) => match response.result {
                        Some(search_session_templates_response::Result::Options(options)) => {
                            vec![ClientEvent::Session(SessionEvent::TemplatesSearched {
                                request_id: response.request_id,
                                session_id,
                                options: options.options,
                            })]
                        }
                        Some(search_session_templates_response::Result::Error(error)) => {
                            vec![ClientEvent::Session(SessionEvent::TemplatesSearchFailed {
                                request_id: response.request_id,
                                session_id,
                                error: ClientError::new(error.message),
                            })]
                        }
                        None => vec![ClientEvent::Session(SessionEvent::TemplatesSearchFailed {
                            request_id,
                            session_id,
                            error: ClientError::new(
                                "session resource search response missing result",
                            ),
                        })],
                    },
                    Err(error) => vec![ClientEvent::Session(SessionEvent::TemplatesSearchFailed {
                        request_id,
                        session_id,
                        error,
                    })],
                },
                ClientEffect::Session(SessionEffect::Watch { open_id }) => {
                    match self.transport.watch_sessions().await {
                        Ok(_) => vec![ClientEvent::Session(SessionEvent::WatchStartedForOpen {
                            open_id,
                        })],
                        Err(error) => vec![ClientEvent::Session(SessionEvent::WatchOpenFailed {
                            open_id,
                            error,
                        })],
                    }
                }
                ClientEffect::Session(SessionEffect::WatchTimeline { key, open_id }) => {
                    match self
                        .transport
                        .watch_timeline(
                            key.session_id.clone(),
                            key.after_cursor,
                            key.channels.clone(),
                            key.initial_limit,
                            key.projection_mode,
                        )
                        .await
                    {
                        Ok(_) => vec![ClientEvent::Session(
                            SessionEvent::TimelineWatchStartedForOpen { key, open_id },
                        )],
                        Err(error) => vec![ClientEvent::Session(
                            SessionEvent::TimelineWatchOpenFailed {
                                key,
                                open_id,
                                error,
                            },
                        )],
                    }
                }
                ClientEffect::Session(SessionEffect::SubmitUserMessage {
                    operation_key,
                    session_id,
                    text,
                }) => match self.transport.submit_user_message(session_id, text).await {
                    Ok(()) => vec![ClientEvent::Session(SessionEvent::UserMessageSubmitted {
                        operation_key,
                    })],
                    Err(error) => vec![ClientEvent::Session(
                        SessionEvent::UserMessageSubmitFailed {
                            operation_key,
                            error,
                        },
                    )],
                },
                ClientEffect::Session(SessionEffect::RespondExternalRequest {
                    operation_key,
                    session_id,
                    request_id,
                    response_json,
                }) => match self
                    .transport
                    .respond_external_request(session_id, request_id, response_json)
                    .await
                {
                    Ok(()) => vec![ClientEvent::Session(
                        SessionEvent::ExternalRequestResponded { operation_key },
                    )],
                    Err(error) => vec![ClientEvent::Session(
                        SessionEvent::ExternalRequestRespondFailed {
                            operation_key,
                            error,
                        },
                    )],
                },
                ClientEffect::Session(SessionEffect::Interrupt {
                    operation_key,
                    session_id,
                }) => match self.transport.interrupt_session(session_id).await {
                    Ok(()) => vec![ClientEvent::Session(SessionEvent::Interrupted {
                        operation_key,
                    })],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::InterruptFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Session(SessionEffect::Rollback {
                    operation_key,
                    session_id,
                    target,
                }) => match self.transport.rollback_session(session_id, target).await {
                    Ok(session) => vec![ClientEvent::Session(SessionEvent::RolledBack {
                        operation_key,
                        session,
                    })],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::RollbackFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Session(SessionEffect::Continue {
                    operation_key,
                    session_id,
                    target,
                }) => match self.transport.continue_session(session_id, target).await {
                    Ok(()) => vec![ClientEvent::Session(SessionEvent::Continued {
                        operation_key,
                    })],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::ContinueFailed {
                        operation_key,
                        error,
                    })],
                },
                ClientEffect::Config(ConfigEffect::SleepReconnect { delay_ms }) => {
                    sleep(delay_ms).await;
                    vec![ClientEvent::Config(ConfigEvent::ReconnectReady)]
                }
                ClientEffect::Status(StatusEffect::SleepReconnect { epoch, delay_ms }) => {
                    sleep(delay_ms).await;
                    vec![ClientEvent::Status(StatusEvent::ReconnectReadyForEpoch {
                        epoch,
                    })]
                }
                ClientEffect::Session(SessionEffect::SleepReconnect { delay_ms }) => {
                    sleep(delay_ms).await;
                    vec![ClientEvent::Session(SessionEvent::ReconnectReady)]
                }
                ClientEffect::Session(SessionEffect::SleepTimelineReconnect { key, delay_ms }) => {
                    sleep(delay_ms).await;
                    vec![ClientEvent::Session(SessionEvent::TimelineReconnectReady {
                        key,
                    })]
                }
                ClientEffect::Routine(RoutineEffect::SleepDefinitionReconnect { delay_ms }) => {
                    sleep(delay_ms).await;
                    vec![ClientEvent::Routine(RoutineEvent::DefinitionReconnectReady)]
                }
                ClientEffect::Routine(RoutineEffect::SleepInvocationReconnect { delay_ms }) => {
                    sleep(delay_ms).await;
                    vec![ClientEvent::Routine(RoutineEvent::InvocationReconnectReady)]
                }
                ClientEffect::Routine(RoutineEffect::SleepSessionInvocationReconnect {
                    session_id,
                    delay_ms,
                }) => {
                    sleep(delay_ms).await;
                    vec![ClientEvent::Routine(
                        RoutineEvent::SessionInvocationReconnectReady { session_id },
                    )]
                }
                ClientEffect::Routine(
                    RoutineEffect::WatchDefinitions { .. } | RoutineEffect::WatchInvocations { .. },
                ) => unreachable!("Routine watches are opened by ClientStateStream"),
            }
        })
    }
}

impl From<ConfigWatchUpdate> for ConfigChange {
    fn from(update: ConfigWatchUpdate) -> Self {
        Self {
            previous_etag: String::new(),
            etag: update.etag.clone(),
            changed_keys: update
                .resources
                .iter()
                .map(|resource| resource.path.clone())
                .collect(),
            error: update.reload_error.clone(),
            config: catalog_config_view(update).ok(),
        }
    }
}

fn catalog_config_view(catalog: ResourceCatalog) -> Result<ConfigView, ClientError> {
    let mut config = catalog
        .effective_configuration
        .ok_or_else(|| ClientError::new("resource catalog missing effective configuration"))?;
    config.etag = catalog.etag;
    Ok(ConfigView::from(config))
}

async fn write_resources(
    transport: &DefaultGrpcTransport,
    writes: Vec<ResourceWrite>,
) -> Result<(), ClientError> {
    for write in writes {
        transport.write_resource(write).await?;
    }
    Ok(())
}

fn reload_failure(error: &ClientError, reload_error: &ClientError) -> ClientError {
    ClientError::new(format!(
        "{error}; authoritative resource catalog reload failed: {reload_error}"
    ))
}
