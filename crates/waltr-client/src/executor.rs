//! Shared async executor for client side-effect intents.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;

use crate::transport::{
    ConfigWatchUpdate, DefaultGrpcTransport, WaltrTransport, SessionTimelineWatchStream,
    SessionWatchStream, StatusWatchStream,
};
use crate::{
    ClientEffect, ClientError, ClientEvent, ConfigEffect, ConfigEvent, ConfigPatch, ConfigView,
    SessionEffect, SessionEvent, SessionSummary, StatusEffect, StatusEvent,
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
    /// Create a gRPC effect executor for an endpoint base URL.
    #[must_use]
    pub fn for_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            transport: DefaultGrpcTransport::new(endpoint),
        }
    }

    /// Load the current configuration as the stable shell-facing view model.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot load configuration.
    pub async fn load_config(&self) -> Result<ConfigView, ClientError> {
        self.transport.get_config().await.map(ConfigView::from)
    }

    /// Patch the current configuration using the stable shell-facing view model.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport rejects or cannot apply the patch.
    pub async fn patch_config(
        &self,
        config: ConfigView,
        expected_etag: impl Into<String>,
    ) -> Result<ConfigView, ClientError> {
        let config: crate::Configuration = config.into();
        self.transport
            .patch_config(ConfigPatch::from(config), expected_etag.into())
            .await
            .map(ConfigView::from)
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
    pub async fn create_session(&self) -> Result<SessionSummary, ClientError> {
        self.transport.create_session().await
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
    ) -> Result<SessionTimelineWatchUpdateStream, ClientError> {
        self.transport
            .watch_timeline(session_id, after_cursor, channels, initial_limit)
            .await
    }

    /// Execute one client side-effect intent and return client events for the reducer.
    #[allow(clippy::too_many_lines)]
    pub async fn execute_effect<Sleep, SleepFuture>(
        &self,
        effect: ClientEffect,
        sleep: Sleep,
    ) -> Vec<ClientEvent>
    where
        Sleep: FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        match effect {
            ClientEffect::Config(ConfigEffect::Load) => match self.transport.get_config().await {
                Ok(config) => vec![ClientEvent::Config(ConfigEvent::Loaded(config))],
                Err(error) => vec![ClientEvent::Config(ConfigEvent::LoadFailed(error))],
            },
            ClientEffect::Config(ConfigEffect::Patch {
                operation_key,
                patch,
                expected_etag,
            }) => match self.transport.patch_config(patch, expected_etag).await {
                Ok(config) => vec![ClientEvent::Config(ConfigEvent::Patched {
                    operation_key,
                    config,
                })],
                Err(error) => vec![ClientEvent::Config(ConfigEvent::PatchFailed {
                    operation_key,
                    error,
                })],
            },
            ClientEffect::Config(ConfigEffect::Watch) => {
                match self.transport.watch_config().await {
                    Ok(_) => vec![ClientEvent::Config(ConfigEvent::WatchStarted)],
                    Err(error) => vec![ClientEvent::Config(ConfigEvent::WatchFailed(error))],
                }
            }
            ClientEffect::Status(StatusEffect::Watch) => {
                match self.transport.watch_status().await {
                    Ok(_) => vec![ClientEvent::Status(StatusEvent::WatchStarted)],
                    Err(error) => vec![ClientEvent::Status(StatusEvent::WatchFailed(error))],
                }
            }
            ClientEffect::Session(SessionEffect::Create { operation_key }) => {
                match self.transport.create_session().await {
                    Ok(session) => vec![ClientEvent::Session(SessionEvent::Created {
                        operation_key,
                        session,
                    })],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::CreateFailed {
                        operation_key,
                        error,
                    })],
                }
            }
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
            ClientEffect::Session(SessionEffect::UpdateRuntimeConfig {
                operation_key,
                id,
                runtime_config,
            }) => {
                match self
                    .transport
                    .update_session_runtime_config(id, *runtime_config)
                    .await
                {
                    Ok(session) => vec![ClientEvent::Session(SessionEvent::RuntimeConfigUpdated {
                        operation_key,
                        session,
                    })],
                    Err(error) => vec![ClientEvent::Session(
                        SessionEvent::RuntimeConfigUpdateFailed {
                            operation_key,
                            error,
                        },
                    )],
                }
            }
            ClientEffect::Session(SessionEffect::List) => {
                match self.transport.list_sessions().await {
                    Ok(sessions) => vec![ClientEvent::Session(SessionEvent::Listed(sessions))],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::ListFailed(error))],
                }
            }
            ClientEffect::Session(SessionEffect::GetTimeline { session_id, query }) => {
                match self.transport.get_timeline(session_id.clone(), query).await {
                    Ok(events) => vec![ClientEvent::Session(SessionEvent::TimelineLoaded {
                        session_id,
                        events,
                    })],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::TimelineLoadFailed {
                        session_id,
                        error,
                    })],
                }
            }
            ClientEffect::Session(SessionEffect::Watch) => {
                match self.transport.watch_sessions().await {
                    Ok(_) => vec![ClientEvent::Session(SessionEvent::WatchStarted)],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::WatchFailed(error))],
                }
            }
            ClientEffect::Session(SessionEffect::WatchTimeline {
                session_id,
                after_cursor,
                channels,
                initial_limit,
            }) => {
                match self
                    .transport
                    .watch_timeline(session_id.clone(), after_cursor, channels, initial_limit)
                    .await
                {
                    Ok(_) => vec![ClientEvent::Session(SessionEvent::TimelineWatchStarted {
                        session_id,
                    })],
                    Err(error) => vec![ClientEvent::Session(SessionEvent::TimelineWatchFailed {
                        session_id,
                        error,
                    })],
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
            ClientEffect::Config(ConfigEffect::SleepReconnect { delay_ms }) => {
                sleep(delay_ms).await;
                vec![ClientEvent::Config(ConfigEvent::ReconnectReady)]
            }
            ClientEffect::Status(StatusEffect::SleepReconnect { delay_ms }) => {
                sleep(delay_ms).await;
                vec![ClientEvent::Status(StatusEvent::ReconnectReady)]
            }
            ClientEffect::Session(SessionEffect::SleepReconnect { delay_ms }) => {
                sleep(delay_ms).await;
                vec![ClientEvent::Session(SessionEvent::ReconnectReady)]
            }
            ClientEffect::Session(SessionEffect::SleepTimelineReconnect {
                session_id,
                delay_ms,
            }) => {
                sleep(delay_ms).await;
                vec![ClientEvent::Session(SessionEvent::TimelineReconnectReady {
                    session_id,
                })]
            }
        }
    }
}

impl From<crate::Configuration> for ConfigPatch {
    fn from(config: crate::Configuration) -> Self {
        Self {
            providers: Some(config.providers),
            mcp_servers: Some(config.mcp_servers),
            runtime_defaults: config.runtime_defaults,
        }
    }
}

impl From<ConfigWatchUpdate> for ConfigChange {
    fn from(update: ConfigWatchUpdate) -> Self {
        Self {
            previous_etag: update.previous_etag,
            etag: update.etag,
            changed_keys: update.changed_keys,
            error: update.error,
            config: update.config.map(ConfigView::from),
        }
    }
}
