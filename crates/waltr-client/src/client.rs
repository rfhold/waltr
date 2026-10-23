//! Shell-neutral client reducer.

use crate::coordination::{access, auth, catalog, config, routine, session, status, stream};
use crate::{
    AccessAttemptId, AccessEvent, AuthEvent, AuthTarget, CatalogEvent, ClientEffect, ClientError,
    ClientEvent, ClientSessionTemplateKind, ClientSessionTemplateSearchTarget, ClientState,
    ConfigEffect, ConfigEvent, Configuration, InferenceDefaults, InferenceSettingsRequest,
    McpServer, OperationKey, Provider, ProviderEditBaseline, ResourceEditBaselineView,
    ResourceKind, ResourceOrigin, ResourceWrite, RoutineEvent, SessionEvent, StatusEvent,
    TimelineQuery,
};

/// Single headless client reducer used by shells.
#[derive(Clone, Debug, Default)]
pub struct Client {
    state: ClientState,
}

impl Client {
    pub(crate) fn clear_mcp_apps(&mut self) {
        self.state.apps = crate::McpAppsSnapshot::default();
        self.state.apps_completed_requests.clear();
    }
    pub(crate) fn retain_completed_app_request(&mut self, request_id: String) {
        crate::apps::retain_completed_request(&mut self.state, request_id);
    }
    pub(crate) fn apps_state_mut(&mut self) -> &mut crate::McpAppsSnapshot {
        &mut self.state.apps
    }
    pub(crate) fn next_app_operation_key(&mut self) -> String {
        self.state.next_operation_key()
    }
    /// Create a client reducer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the current shared client state.
    #[must_use]
    pub const fn state(&self) -> &ClientState {
        &self.state
    }

    pub(crate) fn restore_pending_auth_attempt(&mut self, attempt: crate::AuthAttemptState) {
        self.state.auth.active_attempt = Some(attempt);
    }

    /// Apply a shell-neutral event and return resulting side-effect intents.
    pub fn apply(&mut self, event: ClientEvent) -> ClientUpdate {
        self.apply_current(event)
    }

    pub(crate) fn apply_for_access_generation(
        &mut self,
        access_generation: u64,
        event: ClientEvent,
    ) -> ClientUpdate {
        if self.state.access_generation != access_generation {
            return ClientUpdate::default();
        }
        self.apply_current(event)
    }

    fn apply_current(&mut self, event: ClientEvent) -> ClientUpdate {
        if !event_is_current(&self.state, &event) {
            return ClientUpdate::default();
        }
        if let Some(error) = waltr_login_error(&event) {
            if !matches!(
                self.state.access.status,
                crate::AccessStatus::UnknownChecking | crate::AccessStatus::Authenticated
            ) {
                return ClientUpdate::default();
            }
            return access::reduce(
                &mut self.state,
                AccessEvent::LoginRequired { error: Some(error) },
            );
        }

        match event {
            ClientEvent::McpApp(event) => crate::apps::reduce(&mut self.state, event),
            ClientEvent::Access(event) => access::reduce(&mut self.state, event),
            ClientEvent::Auth(event) => auth::reduce(&mut self.state, event),
            ClientEvent::Catalog(event) => catalog::reduce(&mut self.state, event),
            ClientEvent::Config(event) => config::reduce(&mut self.state, event),
            ClientEvent::Status(event) => status::reduce(&mut self.state, event),
            ClientEvent::Session(event) => session::reduce(&mut self.state, event),
            ClientEvent::Routine(event) => routine::reduce(&mut self.state, event),
            ClientEvent::Stream(event) => {
                stream::reduce(&mut self.state, event);
                ClientUpdate::default()
            }
        }
    }

    /// Record that a platform-specific Waltr login flow started.
    pub fn start_waltr_login(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Access(AccessEvent::LoginStarted))
    }

    /// Record that a platform installed Waltr credentials and restart status checking.
    pub fn complete_waltr_login(&mut self, attempt_id: AccessAttemptId) -> ClientUpdate {
        self.apply(ClientEvent::Access(AccessEvent::LoginCompleted {
            attempt_id,
        }))
    }

    /// Record that a platform-specific Waltr login flow failed.
    pub fn fail_waltr_login(
        &mut self,
        attempt_id: AccessAttemptId,
        error: impl Into<String>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Access(AccessEvent::LoginFailed {
            attempt_id,
            error: error.into(),
        }))
    }

    /// Record that a platform-specific Waltr login flow was canceled.
    pub fn cancel_waltr_login(&mut self, attempt_id: AccessAttemptId) -> ClientUpdate {
        self.apply(ClientEvent::Access(AccessEvent::LoginCanceled {
            attempt_id,
        }))
    }

    /// Reprobe access after native credentials were atomically rejected and cleared.
    pub fn reprobe_waltr_access(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Access(AccessEvent::ReprobeRequested))
    }

    /// Clear protected client state after platform-specific Waltr logout.
    pub fn logout_waltr(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Access(AccessEvent::LoggedOut))
    }

    /// Record proactive native credential expiry or terminal refresh failure.
    pub fn expire_waltr_access(&mut self, error: impl Into<String>) -> ClientUpdate {
        self.apply(ClientEvent::Access(AccessEvent::LoginRequired {
            error: Some(error.into()),
        }))
    }

    /// Request all secret-safe auth statuses.
    pub fn request_auth_statuses(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Auth(AuthEvent::ListRequested))
    }

    /// Begin manual OAuth for a target.
    pub fn begin_auth(&mut self, target: AuthTarget) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Auth(AuthEvent::BeginRequested {
            operation_key,
            target,
        }))
    }

    /// Complete manual OAuth with a pasted redirect URL or code.
    pub fn complete_auth(
        &mut self,
        attempt_id: impl Into<String>,
        input: impl Into<String>,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Auth(AuthEvent::CompleteRequested {
            operation_key,
            attempt_id: attempt_id.into(),
            input: input.into(),
        }))
    }

    /// Start or restart polling for the active server-callback OAuth attempt.
    pub fn poll_auth_attempt(&mut self, attempt_id: impl Into<String>) -> ClientUpdate {
        self.apply(ClientEvent::Auth(AuthEvent::PollRequested {
            attempt_id: attempt_id.into(),
        }))
    }

    /// Cancel a manual OAuth attempt.
    pub fn cancel_auth(&mut self, attempt_id: impl Into<String>) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Auth(AuthEvent::CancelRequested {
            operation_key,
            attempt_id: attempt_id.into(),
        }))
    }

    /// Force-refresh stored OAuth credentials for a target.
    pub fn refresh_auth(&mut self, target: AuthTarget) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Auth(AuthEvent::RefreshRequested {
            operation_key,
            target,
        }))
    }

    /// Delete stored credentials for a target.
    pub fn logout_auth(&mut self, target: AuthTarget) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Auth(AuthEvent::LogoutRequested {
            operation_key,
            target,
        }))
    }

    /// Request that the current configuration be loaded.
    pub fn request_config(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Config(ConfigEvent::LoadRequested))
    }

    /// Request a model catalog search.
    pub fn search_catalog_models(
        &mut self,
        request_id: impl Into<String>,
        query: impl Into<String>,
        provider_id: Option<String>,
        limit: u32,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Catalog(CatalogEvent::ModelsSearchRequested {
            request_id: request_id.into(),
            query: query.into(),
            provider_id,
            limit,
        }))
    }

    /// Request catalog match suggestions for a configured model.
    pub fn suggest_catalog_matches(
        &mut self,
        request_id: impl Into<String>,
        provider_key: impl Into<String>,
        model_key: impl Into<String>,
        limit: u32,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Catalog(
            CatalogEvent::MatchesSuggestRequested {
                request_id: request_id.into(),
                provider_key: provider_key.into(),
                model_key: model_key.into(),
                limit,
            },
        ))
    }

    /// Request that configuration changes be watched.
    #[must_use]
    pub fn register_config_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered))
    }

    /// Release interest in configuration changes.
    #[must_use]
    pub fn release_config_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Config(ConfigEvent::WatchInterestReleased))
    }

    /// Request that a new server-side session be created.
    pub fn create_session(&mut self, agent_name: impl Into<String>) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::CreateRequested {
            operation_key,
            agent_name: agent_name.into(),
        }))
    }

    /// Request that a server-side session be deleted.
    pub fn delete_session(&mut self, id: impl Into<String>) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::DeleteRequested {
            operation_key,
            id: id.into(),
        }))
    }

    /// Request that a server-side session inference settings snapshot be replaced.
    pub fn update_session_inference_settings(
        &mut self,
        id: impl Into<String>,
        inference_settings: InferenceSettingsRequest,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(
            SessionEvent::InferenceSettingsUpdateRequested {
                operation_key,
                id: id.into(),
                inference_settings: Box::new(inference_settings),
            },
        ))
    }

    /// Request that a server-side session switch to an exact selectable agent.
    pub fn update_session_agent(
        &mut self,
        id: impl Into<String>,
        agent_name: impl Into<String>,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(
            SessionEvent::SessionAgentUpdateRequested {
                operation_key,
                id: id.into(),
                agent_name: agent_name.into(),
            },
        ))
    }

    /// Retry loading selectable session agents.
    pub fn request_session_agents(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Session(SessionEvent::AgentsLoadRequested))
    }

    /// Request that the current server-side sessions be loaded.
    pub fn request_sessions(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Session(SessionEvent::ListRequested))
    }

    /// Register interest in the global manual Routine definition watch.
    #[must_use]
    pub fn register_routine_definition_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Routine(
            RoutineEvent::DefinitionWatchInterestRegistered,
        ))
    }

    /// Release interest in the global manual Routine definition watch.
    #[must_use]
    pub fn release_routine_definition_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Routine(
            RoutineEvent::DefinitionWatchInterestReleased,
        ))
    }

    /// Register interest in durable Routine invocation history.
    #[must_use]
    pub fn register_routine_invocation_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Routine(
            RoutineEvent::InvocationWatchInterestRegistered,
        ))
    }

    /// Release interest in durable Routine invocation history.
    #[must_use]
    pub fn release_routine_invocation_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Routine(
            RoutineEvent::InvocationWatchInterestReleased,
        ))
    }

    /// Register interest in authoritative Routine history for one source session.
    #[must_use]
    pub fn register_session_routine_invocation_watch_interest(
        &mut self,
        session_id: impl Into<String>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Routine(
            RoutineEvent::SessionInvocationWatchInterestRegistered {
                session_id: session_id.into(),
            },
        ))
    }

    /// Release interest in authoritative Routine history for one source session.
    #[must_use]
    pub fn release_session_routine_invocation_watch_interest(
        &mut self,
        session_id: impl Into<String>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Routine(
            RoutineEvent::SessionInvocationWatchInterestReleased {
                session_id: session_id.into(),
            },
        ))
    }

    /// Start one manual Routine without automatic retry.
    #[allow(clippy::too_many_arguments)]
    pub fn start_manual_routine(
        &mut self,
        operation_key: OperationKey,
        request_id: impl Into<String>,
        routine_name: impl Into<String>,
        trigger_name: impl Into<String>,
        source_session_id: impl Into<String>,
        inputs: std::collections::BTreeMap<String, crate::ClientRoutineValue>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Routine(RoutineEvent::StartRequested {
            operation_key,
            request_id: request_id.into(),
            routine_name: routine_name.into(),
            trigger_name: trigger_name.into(),
            source_session_id: source_session_id.into(),
            inputs,
        }))
    }

    /// Load the next bounded older-history page when a server cursor is available.
    pub fn load_older_routine_invocations(
        &mut self,
        request_id: impl Into<String>,
        page_size: u32,
    ) -> ClientUpdate {
        let Some(before_admission_sequence) = self.state.routines.older_page_token else {
            return ClientUpdate::default();
        };
        self.apply(ClientEvent::Routine(RoutineEvent::OlderPageRequested {
            request_id: request_id.into(),
            page_size,
            before_admission_sequence,
        }))
    }

    /// Load full task-level detail for one durable invocation.
    pub fn load_routine_invocation_detail(
        &mut self,
        request_id: impl Into<String>,
        invocation_id: impl Into<String>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Routine(RoutineEvent::DetailRequested {
            request_id: request_id.into(),
            invocation_id: invocation_id.into(),
        }))
    }

    /// Request that a session timeline page be loaded.
    pub fn request_session_timeline(
        &mut self,
        session_id: impl Into<String>,
        query: Option<TimelineQuery>,
    ) -> ClientUpdate {
        self.request_session_timeline_with_projection(
            session_id,
            query,
            crate::SessionTimelineProjectionMode::Raw,
        )
    }

    /// Request a timeline page with an explicit server projection.
    pub fn request_session_timeline_with_projection(
        &mut self,
        session_id: impl Into<String>,
        query: Option<TimelineQuery>,
        projection_mode: crate::SessionTimelineProjectionMode,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Session(SessionEvent::TimelineRequested {
            session_id: session_id.into(),
            query,
            projection_mode,
        }))
    }

    /// Request that session changes be watched.
    #[must_use]
    pub fn register_session_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Session(SessionEvent::WatchInterestRegistered))
    }

    /// Release interest in session changes.
    #[must_use]
    pub fn release_session_watch_interest(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Session(SessionEvent::WatchInterestReleased))
    }

    /// Request that timeline changes for one session be watched.
    #[must_use]
    pub fn register_session_timeline_watch_interest(
        &mut self,
        session_id: impl Into<String>,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
    ) -> ClientUpdate {
        self.register_session_timeline_watch_interest_with_projection(
            session_id,
            after_cursor,
            channels,
            initial_limit,
            crate::SessionTimelineProjectionMode::Raw,
        )
    }

    /// Request a timeline watch with an explicit server projection.
    #[must_use]
    pub fn register_session_timeline_watch_interest_with_projection(
        &mut self,
        session_id: impl Into<String>,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
        projection_mode: crate::SessionTimelineProjectionMode,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Session(
            SessionEvent::TimelineWatchInterestRegistered {
                session_id: session_id.into(),
                after_cursor,
                channels,
                initial_limit,
                projection_mode,
            },
        ))
    }

    /// Release interest in timeline changes for one session.
    #[must_use]
    pub fn release_session_timeline_watch_interest(
        &mut self,
        session_id: impl Into<String>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Session(
            SessionEvent::TimelineWatchInterestReleased {
                session_id: session_id.into(),
            },
        ))
    }

    /// Request that a user message be submitted to a session.
    pub fn submit_user_message(
        &mut self,
        session_id: impl Into<String>,
        text: impl Into<String>,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(
            SessionEvent::UserMessageSubmitRequested {
                operation_key,
                session_id: session_id.into(),
                text: text.into(),
            },
        ))
    }

    /// Request that an external request be responded to.
    pub fn respond_external_request(
        &mut self,
        session_id: impl Into<String>,
        request_id: impl Into<String>,
        response_json: impl Into<String>,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(
            SessionEvent::ExternalRequestRespondRequested {
                operation_key,
                session_id: session_id.into(),
                request_id: request_id.into(),
                response_json: response_json.into(),
            },
        ))
    }

    /// Request that an active session run be interrupted.
    pub fn interrupt_session(&mut self, session_id: impl Into<String>) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::InterruptRequested {
            operation_key,
            session_id: session_id.into(),
        }))
    }

    /// Request that a session be rolled back to the end of a turn.
    pub fn rollback_session_to_turn(
        &mut self,
        session_id: impl Into<String>,
        turn: u64,
    ) -> ClientUpdate {
        self.rollback_session(
            session_id,
            crate::RollbackTarget {
                target: Some(crate::rollback_target::Target::Turn(turn)),
            },
        )
    }

    /// Request that a session be rolled back to a specific timeline cursor.
    pub fn rollback_session_to_cursor(
        &mut self,
        session_id: impl Into<String>,
        cursor: u64,
    ) -> ClientUpdate {
        self.rollback_session(
            session_id,
            crate::RollbackTarget {
                target: Some(crate::rollback_target::Target::Cursor(cursor)),
            },
        )
    }

    /// Request that a session be rolled back to immediately before a timeline cursor.
    pub fn rollback_session_before_cursor(
        &mut self,
        session_id: impl Into<String>,
        cursor: u64,
    ) -> ClientUpdate {
        self.rollback_session(
            session_id,
            crate::RollbackTarget {
                target: Some(crate::rollback_target::Target::BeforeCursor(cursor)),
            },
        )
    }

    pub(crate) fn rollback_session(
        &mut self,
        session_id: impl Into<String>,
        target: crate::RollbackTarget,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::RollbackRequested {
            operation_key,
            session_id: session_id.into(),
            target,
        }))
    }

    /// Request that a session roll back to a turn and continue from there.
    pub fn continue_session_from_turn(
        &mut self,
        session_id: impl Into<String>,
        turn: u64,
    ) -> ClientUpdate {
        self.continue_session(
            session_id,
            crate::RollbackTarget {
                target: Some(crate::rollback_target::Target::Turn(turn)),
            },
        )
    }

    /// Request that a session roll back to a cursor and continue from there.
    pub fn continue_session_from_cursor(
        &mut self,
        session_id: impl Into<String>,
        cursor: u64,
    ) -> ClientUpdate {
        self.continue_session(
            session_id,
            crate::RollbackTarget {
                target: Some(crate::rollback_target::Target::Cursor(cursor)),
            },
        )
    }

    pub(crate) fn continue_session(
        &mut self,
        session_id: impl Into<String>,
        target: crate::RollbackTarget,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::ContinueRequested {
            operation_key,
            session_id: session_id.into(),
            target,
        }))
    }

    /// Request that system status changes be watched.
    pub fn request_status_watch(&mut self) -> ClientUpdate {
        if !self.state.permits_protected_requests() {
            return ClientUpdate::default();
        }
        self.apply(ClientEvent::Status(StatusEvent::WatchRequested))
    }

    /// Maintain registered watch streams according to shared coordination rules.
    #[must_use]
    pub fn maintain_watches(&mut self) -> ClientUpdate {
        if status::needs_watch_request(&self.state) {
            return self.request_status_watch();
        }

        if self.state.config.watch.interests > 0 {
            let update = config::maintain_watch(&mut self.state);
            if !update.is_empty() {
                return update;
            }
        }

        if self.state.sessions.watch.interests > 0 {
            let update = session::maintain_watch(&mut self.state);
            if !update.is_empty() {
                return update;
            }
        }

        if self.state.routines.definition_watch.interests > 0 {
            let update = routine::maintain_definition_watch(&mut self.state);
            if !update.is_empty() {
                return update;
            }
        }

        if self.state.routines.invocation_watch.interests > 0 {
            let update = routine::maintain_invocation_watch(&mut self.state);
            if !update.is_empty() {
                return update;
            }
        }

        let session_ids = self
            .state
            .routines
            .session_histories
            .iter()
            .filter(|(_, history)| history.watch.interests > 0)
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            let update = routine::maintain_session_invocation_watch(&mut self.state, &session_id);
            if !update.is_empty() {
                return update;
            }
        }

        for key in self.state.timeline_watch_keys() {
            let update = session::maintain_timeline_watch(&mut self.state, &key);
            if !update.is_empty() {
                return update;
            }
        }

        ClientUpdate::default()
    }

    /// Request that the latest configuration be reset to server defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded.
    pub fn reset_config(&mut self) -> Result<ClientUpdate, ClientError> {
        self.current_config()?;
        let catalog = self
            .state
            .config
            .catalog
            .as_ref()
            .ok_or_else(|| ClientError::new("resource catalog not loaded"))?;
        let writes = catalog
            .resources
            .iter()
            .filter(|resource| {
                matches!(
                    ResourceKind::try_from(resource.kind),
                    Ok(ResourceKind::Provider | ResourceKind::McpServer | ResourceKind::Defaults)
                ) && ResourceOrigin::try_from(resource.origin) == Ok(ResourceOrigin::Filesystem)
            })
            .map(|resource| ResourceWrite::Reset {
                path: resource.path.clone(),
                expected_etag: resource.persisted_etag.clone(),
            })
            .collect();
        let operation_key = self.state.next_operation_key();
        let mut update = self.apply(ClientEvent::Config(ConfigEvent::ResetRequested(
            operation_key.clone(),
        )));
        update
            .effects
            .push(ClientEffect::Config(ConfigEffect::Reset {
                operation_key,
                writes,
            }));
        Ok(update)
    }

    fn request_resource_writes(&mut self, writes: Vec<ResourceWrite>) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        let mut update = self.apply(ClientEvent::Config(ConfigEvent::PatchRequested(
            operation_key.clone(),
        )));
        update
            .effects
            .push(ClientEffect::Config(ConfigEffect::Patch {
                operation_key,
                writes,
            }));
        update
    }

    /// Request a patch that adds a provider to the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or provider names are invalid.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "command API owns submitted values"
    )]
    pub fn add_provider(&mut self, provider: Provider) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        Self::validate_provider_name(&provider.name)?;
        let name = provider.name.clone();
        if config.providers.contains_key(&name) {
            return Err(ClientError::new(format!(
                "provider name '{name}' is already used"
            )));
        }
        let expected_etag = self.absent_slot_etag()?;
        let source = crate::resource::provider_source(&provider, None, None)?;
        Ok(self.request_resource_writes(vec![ResourceWrite::Replace {
            path: provider_path(&name),
            source,
            expected_etag,
        }]))
    }

    /// Request a patch that replaces a provider in the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded, the provider is missing, or names are invalid.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "command API owns submitted values"
    )]
    pub fn update_provider(
        &mut self,
        original_name: &str,
        original: Provider,
        provider: Provider,
        baseline: &ResourceEditBaselineView,
    ) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        Self::validate_provider_name(original_name)?;
        Self::validate_provider_name(&provider.name)?;
        let provider_name = provider.name.clone();
        if provider_name != original_name && config.providers.contains_key(&provider_name) {
            return Err(ClientError::new(format!(
                "provider name '{provider_name}' is already used"
            )));
        }

        let source_path = provider_path(original_name);
        Self::validate_edit_baseline(&source_path, baseline)?;
        let source = crate::resource::provider_source(
            &provider,
            Some(&original),
            Some(&baseline.active_source),
        )?;
        let mut writes = Vec::new();
        if provider_name == original_name {
            writes.push(ResourceWrite::Replace {
                path: source_path,
                source,
                expected_etag: baseline.persisted_etag.clone(),
            });
        } else {
            writes.push(ResourceWrite::Replace {
                path: provider_path(&provider_name),
                source,
                expected_etag: baseline.absent_etag.clone(),
            });
            writes.push(ResourceWrite::Reset {
                path: source_path,
                expected_etag: baseline.persisted_etag.clone(),
            });
        }
        Ok(self.request_resource_writes(writes))
    }

    /// Request a patch that deletes a provider from the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded, the provider is missing, or names are invalid.
    pub fn delete_provider(&mut self, name: &str) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        if !config.providers.contains_key(name) {
            return Err(ClientError::new(format!("provider '{name}' not found")));
        }

        let path = provider_path(name);
        let baseline = self.resource_baseline(&path)?;
        Ok(self.request_resource_writes(vec![ResourceWrite::Reset {
            path,
            expected_etag: baseline.persisted_etag.clone(),
        }]))
    }

    /// Request a patch that adds an MCP server to the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or the server key is invalid.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "command API owns submitted values"
    )]
    pub fn add_mcp_server(&mut self, server: McpServer) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        Self::validate_mcp_server_key(&server.name)?;
        let key = server.name.clone();
        if config.mcp_servers.contains_key(&key) {
            return Err(ClientError::new(format!(
                "MCP server key '{key}' is already used"
            )));
        }
        let expected_etag = self.absent_slot_etag()?;
        let source = crate::resource::mcp_server_source(&server, None, None)?;
        Ok(self.request_resource_writes(vec![ResourceWrite::Replace {
            path: mcp_server_path(&key),
            source,
            expected_etag,
        }]))
    }

    /// Request a patch that replaces an MCP server in the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded, the server is missing, or keys are invalid.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "command API owns submitted values"
    )]
    pub fn update_mcp_server(
        &mut self,
        original_name: &str,
        original: McpServer,
        server: McpServer,
        baseline: &ResourceEditBaselineView,
    ) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        Self::validate_mcp_server_key(original_name)?;
        Self::validate_mcp_server_key(&server.name)?;
        let server_key = server.name.clone();
        if server_key != original_name && config.mcp_servers.contains_key(&server_key) {
            return Err(ClientError::new(format!(
                "MCP server key '{server_key}' is already used"
            )));
        }

        let source_path = mcp_server_path(original_name);
        Self::validate_edit_baseline(&source_path, baseline)?;
        let source = crate::resource::mcp_server_source(
            &server,
            Some(&original),
            Some(&baseline.active_source),
        )?;
        let mut writes = Vec::new();
        if server_key == original_name {
            writes.push(ResourceWrite::Replace {
                path: source_path,
                source,
                expected_etag: baseline.persisted_etag.clone(),
            });
        } else {
            writes.push(ResourceWrite::Replace {
                path: mcp_server_path(&server_key),
                source,
                expected_etag: baseline.absent_etag.clone(),
            });
            writes.push(ResourceWrite::Reset {
                path: source_path,
                expected_etag: baseline.persisted_etag.clone(),
            });
        }
        Ok(self.request_resource_writes(writes))
    }

    /// Request a patch that deletes an MCP server from the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or the server is missing.
    pub fn delete_mcp_server(&mut self, name: &str) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        if !config.mcp_servers.contains_key(name) {
            return Err(ClientError::new(format!("MCP server '{name}' not found")));
        }

        let path = mcp_server_path(name);
        let baseline = self.resource_baseline(&path)?;
        Ok(self.request_resource_writes(vec![ResourceWrite::Reset {
            path,
            expected_etag: baseline.persisted_etag.clone(),
        }]))
    }

    /// Request a patch that replaces inference defaults in the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or defaults reference missing config.
    pub fn set_inference_defaults(
        &mut self,
        defaults: Option<InferenceDefaults>,
    ) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        if let Some(defaults) = defaults.as_ref() {
            Self::validate_inference_defaults(defaults, &config)?;
        }

        let path = "defaults/default.yaml";
        let baseline = self.resource_baseline(path)?;
        let write = if let Some(defaults) = defaults {
            let source = crate::resource::defaults_source(
                &defaults,
                config.inference_defaults.as_ref(),
                Some(&baseline.active_source),
            )?;
            ResourceWrite::Replace {
                path: path.to_owned(),
                source,
                expected_etag: baseline.persisted_etag.clone(),
            }
        } else {
            ResourceWrite::Reset {
                path: path.to_owned(),
                expected_etag: baseline.persisted_etag.clone(),
            }
        };
        Ok(self.request_resource_writes(vec![write]))
    }

    /// Request a patch that renames a provider in the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded, the provider is missing, or names are invalid.
    pub fn rename_provider(
        &mut self,
        original_name: &str,
        new_name: &str,
    ) -> Result<ClientUpdate, ClientError> {
        let mut config = self.current_config()?;
        Self::validate_provider_name(new_name)?;
        if new_name != original_name && config.providers.contains_key(new_name) {
            return Err(ClientError::new(format!(
                "provider name '{new_name}' is already used"
            )));
        }

        let mut provider = config
            .providers
            .remove(original_name)
            .ok_or_else(|| ClientError::new(format!("provider '{original_name}' not found")))?;

        let original = provider.clone();
        new_name.clone_into(&mut provider.name);
        let source_path = provider_path(original_name);
        let source_baseline = self.resource_baseline(&source_path)?;
        let source = crate::resource::provider_source(
            &provider,
            Some(&original),
            Some(&source_baseline.active_source),
        )?;
        Ok(self.request_resource_writes(vec![
            ResourceWrite::Replace {
                path: provider_path(new_name),
                source,
                expected_etag: self.absent_slot_etag()?,
            },
            ResourceWrite::Reset {
                path: source_path,
                expected_etag: source_baseline.persisted_etag.clone(),
            },
        ]))
    }

    /// Return a provider edit baseline from the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or the provider is missing.
    pub fn provider_edit_baseline(&self, name: &str) -> Result<ProviderEditBaseline, ClientError> {
        let config = self
            .state
            .config
            .current
            .as_ref()
            .ok_or_else(|| ClientError::new("configuration not loaded"))?;
        let provider = config
            .providers
            .get(name)
            .ok_or_else(|| ClientError::new(format!("provider '{name}' not found")))?;
        let resource = self.resource_baseline(&provider_path(name))?;

        Ok(ProviderEditBaseline {
            name: name.to_owned(),
            persisted_etag: resource.persisted_etag.clone(),
            active_source: resource.active_source.clone(),
            provider: provider.clone(),
        })
    }

    /// Request template search for one session.
    #[allow(clippy::too_many_arguments)]
    pub fn search_session_templates(
        &mut self,
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        query: impl Into<String>,
        limit: u32,
        kinds: Vec<ClientSessionTemplateKind>,
        target: ClientSessionTemplateSearchTarget,
        cursor: Option<u32>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Session(
            SessionEvent::TemplatesSearchRequested {
                request_id: request_id.into(),
                session_id: session_id.into(),
                query: query.into(),
                limit,
                kinds: kinds.into_iter().map(session_template_kind).collect(),
                target: session_template_search_target(target),
                cursor,
            },
        ))
    }

    /// Request deletion of every server-side session after shell-local confirmation.
    pub fn wipe_sessions(&mut self) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::WipeRequested(
            operation_key,
        )))
    }

    /// Return whether the provider has changed since the edit baseline.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or the provider is missing.
    pub fn provider_changed_since(
        &self,
        baseline: &ProviderEditBaseline,
    ) -> Result<bool, ClientError> {
        let config = self
            .state
            .config
            .current
            .as_ref()
            .ok_or_else(|| ClientError::new("configuration not loaded"))?;
        let provider = config.providers.get(&baseline.name);

        Ok(provider != Some(&baseline.provider))
    }

    fn current_config(&self) -> Result<Configuration, ClientError> {
        self.state
            .config
            .current
            .clone()
            .ok_or_else(|| ClientError::new("configuration not loaded"))
    }

    fn resource_baseline(&self, path: &str) -> Result<&crate::Resource, ClientError> {
        self.state
            .config
            .catalog
            .as_ref()
            .ok_or_else(|| ClientError::new("resource catalog not loaded"))?
            .resources
            .iter()
            .find(|resource| resource.path == path)
            .ok_or_else(|| ClientError::new(format!("resource baseline '{path}' not found")))
    }

    fn absent_slot_etag(&self) -> Result<String, ClientError> {
        self.state
            .config
            .catalog
            .as_ref()
            .ok_or_else(|| ClientError::new("resource catalog not loaded"))?
            .resources
            .iter()
            .find(|resource| {
                ResourceOrigin::try_from(resource.origin) == Ok(ResourceOrigin::Bundled)
                    && !resource.persisted_etag.is_empty()
            })
            .map(|resource| resource.persisted_etag.clone())
            .ok_or_else(|| ClientError::new("absent resource-slot baseline not found"))
    }

    fn validate_edit_baseline(
        expected_path: &str,
        baseline: &ResourceEditBaselineView,
    ) -> Result<(), ClientError> {
        if baseline.path != expected_path {
            return Err(ClientError::new(format!(
                "resource baseline '{}' does not match '{expected_path}'",
                baseline.path
            )));
        }
        if baseline.persisted_etag.is_empty()
            || baseline.absent_etag.is_empty()
            || baseline.active_source.is_empty()
        {
            return Err(ClientError::new(format!(
                "resource baseline '{expected_path}' is incomplete"
            )));
        }
        Ok(())
    }

    fn validate_provider_name(name: &str) -> Result<(), ClientError> {
        if name.is_empty() {
            return Err(ClientError::new("provider name cannot be empty"));
        }
        if name.chars().any(char::is_whitespace) {
            return Err(ClientError::new(format!(
                "provider name '{name}' cannot contain spaces"
            )));
        }
        if name.chars().any(char::is_uppercase) {
            return Err(ClientError::new(format!(
                "provider name '{name}' must be lowercase"
            )));
        }
        if !valid_resource_name(name) {
            return Err(ClientError::new(format!(
                "provider name '{name}' is invalid"
            )));
        }

        Ok(())
    }

    fn validate_mcp_server_key(name: &str) -> Result<(), ClientError> {
        if name.is_empty() {
            return Err(ClientError::new("MCP server key cannot be empty"));
        }
        if name.chars().any(char::is_whitespace) {
            return Err(ClientError::new(format!(
                "MCP server key '{name}' cannot contain spaces"
            )));
        }
        if name.chars().any(char::is_uppercase) {
            return Err(ClientError::new(format!(
                "MCP server key '{name}' must be lowercase"
            )));
        }
        if !valid_resource_name(name) {
            return Err(ClientError::new(format!(
                "MCP server key '{name}' is invalid"
            )));
        }

        Ok(())
    }

    fn validate_inference_defaults(
        defaults: &InferenceDefaults,
        config: &Configuration,
    ) -> Result<(), ClientError> {
        if defaults.provider_key.trim().is_empty() && defaults.model_key.trim().is_empty() {
            return Ok(());
        }

        let provider = config
            .providers
            .get(&defaults.provider_key)
            .ok_or_else(|| {
                ClientError::new(format!(
                    "inference default provider '{}' not found",
                    defaults.provider_key
                ))
            })?;
        if defaults.model_key.trim().is_empty() {
            return Err(ClientError::new(
                "inference default model id cannot be empty when provider is set",
            ));
        }
        let model = provider.models.get(&defaults.model_key).ok_or_else(|| {
            ClientError::new(format!(
                "inference default model '{}' not found in provider '{}'",
                defaults.model_key, defaults.provider_key
            ))
        })?;
        if let Some(effort) = defaults
            .reasoning_effort
            .as_ref()
            .map(|effort| effort.trim())
            .filter(|effort| !effort.is_empty())
        {
            let supported = model.options.as_ref().is_some_and(|options| {
                options
                    .reasoning_efforts
                    .iter()
                    .any(|value| value == effort)
            });
            if !supported {
                return Err(ClientError::new(format!(
                    "inference default reasoning effort '{effort}' not found in model '{}' for provider '{}'",
                    defaults.model_key, defaults.provider_key
                )));
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
fn event_is_current(state: &ClientState, event: &ClientEvent) -> bool {
    match event {
        ClientEvent::McpApp(crate::McpAppEvent::Failed { request_id, .. }) => state
            .apps
            .requests
            .get(request_id)
            .is_some_and(|op| op.status == "loading"),
        ClientEvent::Status(
            StatusEvent::WatchStartedForEpoch { epoch }
            | StatusEvent::WatchChangedForEpoch { epoch, .. }
            | StatusEvent::WatchFailedForEpoch { epoch, .. }
            | StatusEvent::WatchEndedForEpoch { epoch }
            | StatusEvent::ReconnectReadyForEpoch { epoch },
        ) => state.status.watch_epoch == *epoch,
        ClientEvent::Auth(
            AuthEvent::Begun { operation_key, .. } | AuthEvent::BeginFailed { operation_key, .. },
        ) => operation_is_current(state.auth.begin.key.as_ref(), operation_key),
        ClientEvent::Auth(
            AuthEvent::Completed { operation_key, .. }
            | AuthEvent::CompleteFailed { operation_key, .. },
        ) => operation_is_current(state.auth.complete.key.as_ref(), operation_key),
        ClientEvent::Auth(
            AuthEvent::Canceled { operation_key } | AuthEvent::CancelFailed { operation_key, .. },
        ) => operation_is_current(state.auth.cancel.key.as_ref(), operation_key),
        ClientEvent::Auth(
            AuthEvent::Refreshed { operation_key, .. }
            | AuthEvent::RefreshFailed { operation_key, .. },
        ) => matches!(
            state.operations.get(operation_key),
            Some(crate::ClientOperationState::AuthRefresh {
                status: crate::OperationStatus::Loading,
                ..
            })
        ),
        ClientEvent::Auth(
            AuthEvent::LoggedOut { operation_key, .. }
            | AuthEvent::LogoutFailed { operation_key, .. },
        ) => operation_is_current(state.auth.logout.key.as_ref(), operation_key),
        ClientEvent::Auth(
            AuthEvent::Polled {
                attempt_id,
                poll_generation,
                ..
            }
            | AuthEvent::PollFailed {
                attempt_id,
                poll_generation,
                ..
            },
        ) => state.auth_poll_is_current(attempt_id, *poll_generation),
        ClientEvent::Catalog(
            CatalogEvent::ModelsSearched { request_id, .. }
            | CatalogEvent::ModelsSearchFailed { request_id, .. },
        ) => state.catalog.search.request_id == *request_id,
        ClientEvent::Catalog(
            CatalogEvent::MatchesSuggested { request_id, .. }
            | CatalogEvent::MatchesSuggestFailed { request_id, .. },
        ) => state.catalog.suggestions.request_id == *request_id,
        ClientEvent::Config(
            ConfigEvent::Patched { operation_key, .. }
            | ConfigEvent::PatchFailed { operation_key, .. },
        ) => operation_is_current(state.config.patch.key.as_ref(), operation_key),
        ClientEvent::Config(
            ConfigEvent::Reset { operation_key, .. }
            | ConfigEvent::ResetFailed { operation_key, .. },
        ) => operation_is_current(state.config.reset.key.as_ref(), operation_key),
        ClientEvent::Config(
            ConfigEvent::WatchStartedForOpen { open_id }
            | ConfigEvent::WatchOpenFailed { open_id, .. },
        ) => state.config.watch.open_id == Some(*open_id),
        ClientEvent::Session(
            SessionEvent::Created { operation_key, .. }
            | SessionEvent::CreateFailed { operation_key, .. },
        ) => operation_is_current(state.sessions.create.key.as_ref(), operation_key),
        ClientEvent::Session(
            SessionEvent::Deleted { operation_key, .. }
            | SessionEvent::DeleteFailed { operation_key, .. },
        ) => operation_is_current(state.sessions.delete.key.as_ref(), operation_key),
        ClientEvent::Session(
            SessionEvent::Wiped { operation_key } | SessionEvent::WipeFailed { operation_key, .. },
        ) => operation_is_current(state.sessions.wipe.key.as_ref(), operation_key),
        ClientEvent::Session(
            SessionEvent::InferenceSettingsUpdated { operation_key, .. }
            | SessionEvent::InferenceSettingsUpdateFailed { operation_key, .. },
        ) => operation_is_current(
            state.sessions.inference_settings_update.key.as_ref(),
            operation_key,
        ),
        ClientEvent::Session(
            SessionEvent::UserMessageSubmitted { operation_key }
            | SessionEvent::UserMessageSubmitFailed { operation_key, .. },
        ) => operation_is_current(
            state.sessions.submit_user_message.key.as_ref(),
            operation_key,
        ),
        ClientEvent::Session(
            SessionEvent::ExternalRequestResponded { operation_key }
            | SessionEvent::ExternalRequestRespondFailed { operation_key, .. },
        ) => operation_is_current(
            state.sessions.respond_external_request.key.as_ref(),
            operation_key,
        ),
        ClientEvent::Session(
            SessionEvent::Interrupted { operation_key }
            | SessionEvent::InterruptFailed { operation_key, .. },
        ) => operation_is_current(state.sessions.interrupt.key.as_ref(), operation_key),
        ClientEvent::Session(
            SessionEvent::RolledBack { operation_key, .. }
            | SessionEvent::RollbackFailed { operation_key, .. },
        ) => operation_is_current(state.sessions.rollback.key.as_ref(), operation_key),
        ClientEvent::Session(
            SessionEvent::Continued { operation_key }
            | SessionEvent::ContinueFailed { operation_key, .. },
        ) => operation_is_current(state.sessions.continue_session.key.as_ref(), operation_key),
        ClientEvent::Session(
            SessionEvent::WatchStartedForOpen { open_id }
            | SessionEvent::WatchOpenFailed { open_id, .. },
        ) => state.sessions.watch.open_id == Some(*open_id),
        ClientEvent::Routine(
            RoutineEvent::DefinitionWatchStartedForOpen { open_id }
            | RoutineEvent::DefinitionWatchOpenFailed { open_id, .. },
        ) => state.routines.definition_watch.open_id == Some(*open_id),
        ClientEvent::Routine(
            RoutineEvent::InvocationWatchStartedForOpen { open_id }
            | RoutineEvent::InvocationWatchOpenFailed { open_id, .. },
        ) => state.routines.invocation_watch.open_id == Some(*open_id),
        ClientEvent::Session(
            SessionEvent::TimelineWatchStartedForOpen { key, open_id }
            | SessionEvent::TimelineWatchOpenFailed { key, open_id, .. },
        ) => state
            .sessions
            .timelines
            .get(&key.session_id)
            .and_then(|timeline| timeline.watches.get(key))
            .is_some_and(|watch| watch.open_id == Some(*open_id)),
        ClientEvent::Session(
            SessionEvent::TemplatesSearched {
                request_id,
                session_id,
                ..
            }
            | SessionEvent::TemplatesSearchFailed {
                request_id,
                session_id,
                ..
            },
        ) => state
            .sessions
            .template_searches
            .get(session_id)
            .is_some_and(|search| search.request_id == *request_id),
        _ => true,
    }
}

fn operation_is_current(current: Option<&OperationKey>, completed: &OperationKey) -> bool {
    current.is_none_or(|current| current == completed)
}

#[allow(clippy::manual_let_else)]
fn waltr_login_error(event: &ClientEvent) -> Option<String> {
    let error = match event {
        ClientEvent::McpApp(crate::McpAppEvent::Failed { error, .. }) => error,
        ClientEvent::Auth(
            AuthEvent::ListFailed(error)
            | AuthEvent::BeginFailed { error, .. }
            | AuthEvent::CompleteFailed { error, .. }
            | AuthEvent::CancelFailed { error, .. }
            | AuthEvent::RefreshFailed { error, .. }
            | AuthEvent::LogoutFailed { error, .. },
        )
        | ClientEvent::Catalog(
            CatalogEvent::ModelsSearchFailed { error, .. }
            | CatalogEvent::MatchesSuggestFailed { error, .. },
        )
        | ClientEvent::Config(
            ConfigEvent::LoadFailed(error)
            | ConfigEvent::PatchFailed { error, .. }
            | ConfigEvent::ResetFailed { error, .. }
            | ConfigEvent::WatchFailed(error)
            | ConfigEvent::WatchOpenFailed { error, .. },
        )
        | ClientEvent::Status(
            StatusEvent::WatchFailed(error) | StatusEvent::WatchFailedForEpoch { error, .. },
        )
        | ClientEvent::Session(
            SessionEvent::CreateFailed { error, .. }
            | SessionEvent::DeleteFailed { error, .. }
            | SessionEvent::WipeFailed { error, .. }
            | SessionEvent::InferenceSettingsUpdateFailed { error, .. }
            | SessionEvent::ListFailed(error)
            | SessionEvent::TimelineLoadFailed { error, .. }
            | SessionEvent::TemplatesSearchFailed { error, .. }
            | SessionEvent::TimelineWatchFailed { error, .. }
            | SessionEvent::TimelineWatchOpenFailed { error, .. }
            | SessionEvent::UserMessageSubmitFailed { error, .. }
            | SessionEvent::ExternalRequestRespondFailed { error, .. }
            | SessionEvent::InterruptFailed { error, .. }
            | SessionEvent::RollbackFailed { error, .. }
            | SessionEvent::ContinueFailed { error, .. }
            | SessionEvent::WatchFailed(error)
            | SessionEvent::WatchOpenFailed { error, .. },
        )
        | ClientEvent::Routine(
            RoutineEvent::DefinitionWatchOpenFailed { error, .. }
            | RoutineEvent::DefinitionWatchFailed(error)
            | RoutineEvent::InvocationWatchOpenFailed { error, .. }
            | RoutineEvent::InvocationWatchFailed(error)
            | RoutineEvent::StartFailed { error, .. }
            | RoutineEvent::OlderPageFailed { error, .. }
            | RoutineEvent::DetailFailed { error, .. },
        ) => error,
        _ => return None,
    };

    error
        .is_waltr_login_required()
        .then(|| error.message().to_owned())
}

fn valid_resource_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn provider_path(name: &str) -> String {
    format!("providers/{name}.yaml")
}

fn mcp_server_path(name: &str) -> String {
    format!("mcp-servers/{name}.yaml")
}

/// Result of applying an event to the shared client.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientUpdate {
    effects: Vec<ClientEffect>,
}

impl ClientUpdate {
    /// Create an update containing one side-effect intent.
    #[must_use]
    pub fn with_effect(effect: ClientEffect) -> Self {
        Self {
            effects: vec![effect],
        }
    }

    /// Return emitted side-effect intents.
    #[must_use]
    pub fn effects(&self) -> &[ClientEffect] {
        &self.effects
    }

    /// Return true when no side effects were emitted.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    pub(crate) fn extend(&mut self, update: Self) {
        self.effects.extend(update.effects);
    }
}

const fn session_template_kind(kind: ClientSessionTemplateKind) -> crate::SessionTemplateKind {
    match kind {
        ClientSessionTemplateKind::File => crate::SessionTemplateKind::File,
        ClientSessionTemplateKind::Tool => crate::SessionTemplateKind::Tool,
        ClientSessionTemplateKind::Resource => crate::SessionTemplateKind::Resource,
        ClientSessionTemplateKind::Prompt => crate::SessionTemplateKind::Prompt,
    }
}

const fn session_template_search_target(
    target: ClientSessionTemplateSearchTarget,
) -> crate::SessionTemplateSearchTarget {
    match target {
        ClientSessionTemplateSearchTarget::Resources => {
            crate::SessionTemplateSearchTarget::Resources
        }
        ClientSessionTemplateSearchTarget::Prompts => crate::SessionTemplateSearchTarget::Prompts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::transport::{SessionTimelineWatchEvent, SessionTimelineWatchUpdate};
    use crate::{
        AccessStatus, CatalogEffect, CatalogEvent, CatalogMatch, CatalogMatchConfidence,
        CatalogModelRef, CatalogModelSummary, ConfigWatchUpdate, OperationStatus, SessionEffect,
        SessionSummary, SessionTemplateKind, SessionTemplateOption, SessionWatchEvent,
        SessionWatchUpdate, StatusEffect, StreamEvent, StreamStatus, SystemStatus, TimelineEvent,
        watch_reconnect_delay_ms,
    };

    #[test]
    fn waltr_access_login_transitions_are_independent_from_provider_auth() {
        let mut client = Client::new();

        assert_eq!(client.state().access.status, AccessStatus::UnknownChecking);
        client.start_waltr_login();
        let failed_attempt = client.state().access.active_attempt_id.clone().unwrap();
        assert_eq!(client.state().access.status, AccessStatus::LoginInProgress);

        client.fail_waltr_login(failed_attempt, "identity provider unavailable");
        assert_eq!(client.state().access.status, AccessStatus::LoginFailed);
        assert_eq!(
            client.state().access.error.as_deref(),
            Some("identity provider unavailable")
        );

        client.start_waltr_login();
        let canceled_attempt = client.state().access.active_attempt_id.clone().unwrap();
        client.cancel_waltr_login(canceled_attempt);
        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);

        client.start_waltr_login();
        let completed_attempt = client.state().access.active_attempt_id.clone().unwrap();
        client.complete_waltr_login(completed_attempt);
        let epoch = client.state().status.watch_epoch;
        client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch,
            status: SystemStatus { ready: true },
        }));
        assert_eq!(client.state().access.status, AccessStatus::Authenticated);
    }

    #[test]
    fn waltr_access_loss_suspends_watches_without_reconnect_and_preserves_interest() {
        let mut client = Client::new();
        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.register_config_watch_interest());
        drop(client.register_session_watch_interest());
        drop(client.register_session_timeline_watch_interest("session", Some(0), Vec::new(), None));

        let update = client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::waltr_login_required("unauthenticated: login required"),
        )));

        assert!(update.is_empty());
        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);
        assert_eq!(client.state().status.watch.status, OperationStatus::Waiting);
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Waiting
        );
        assert!(client.maintain_watches().is_empty());
        assert!(
            client
                .apply(ClientEvent::Status(StatusEvent::ReconnectReady))
                .is_empty()
        );
        assert_eq!(client.state().config.watch.interests, 1);
        assert_eq!(client.state().sessions.watch.interests, 1);
        assert_eq!(client.state().timeline_watch_keys().len(), 1);
    }

    #[test]
    fn transient_reprobe_preserves_interests_reload_flags_and_resumes_watches() {
        let mut client = Client::new();
        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.register_config_watch_interest());
        drop(client.register_session_watch_interest());
        drop(client.register_session_timeline_watch_interest("session", Some(0), Vec::new(), None));
        client.apply(ClientEvent::Config(ConfigEvent::Loaded(catalog(
            config_with_provider("anthropic", "etag-1"),
        ))));
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "session",
        )])));

        client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::waltr_login_required("current credential rejected"),
        )));
        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);
        assert_eq!(client.state().config.watch.interests, 1);
        assert_eq!(client.state().sessions.watch.interests, 1);
        assert_eq!(client.state().timeline_watch_keys().len(), 1);
        assert!(client.state().access.reload_config);
        assert!(client.state().access.reload_sessions);

        let update = client.reprobe_waltr_access();
        assert_eq!(client.state().access.status, AccessStatus::UnknownChecking);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::Watch { .. })]
        ));
        assert_eq!(client.state().config.watch.interests, 1);
        assert_eq!(client.state().sessions.watch.interests, 1);
        assert_eq!(client.state().timeline_watch_keys().len(), 1);

        let epoch = client.state().status.watch_epoch;
        let update = client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch,
            status: SystemStatus { ready: true },
        }));
        assert_eq!(client.state().access.status, AccessStatus::Authenticated);
        assert!(matches!(
            update.effects(),
            [
                ClientEffect::Config(ConfigEffect::Watch { .. }),
                ClientEffect::Session(SessionEffect::Watch { .. }),
                ClientEffect::Session(SessionEffect::ListAgents { .. }),
                ClientEffect::Config(ConfigEffect::Load),
                ClientEffect::Session(SessionEffect::List),
            ]
        ));
        assert!(matches!(
            client.maintain_watches().effects(),
            [ClientEffect::Session(SessionEffect::WatchTimeline { .. })]
        ));
        assert_eq!(client.state().config.watch.interests, 1);
        assert_eq!(client.state().sessions.watch.interests, 1);
        assert_eq!(client.state().timeline_watch_keys().len(), 1);
        assert!(!client.state().access.reload_config);
        assert!(!client.state().access.reload_sessions);
    }

    #[test]
    fn completed_waltr_login_restarts_status_before_interested_child_watches() {
        let mut client = Client::new();
        drop(client.register_config_watch_interest());
        drop(client.register_session_watch_interest());
        drop(client.register_session_timeline_watch_interest("session", Some(0), Vec::new(), None));
        client.apply(ClientEvent::Access(AccessEvent::LoginRequired {
            error: None,
        }));
        client.start_waltr_login();
        let attempt_id = client.state().access.active_attempt_id.clone().unwrap();

        let update = client.complete_waltr_login(attempt_id);
        assert_eq!(client.state().access.status, AccessStatus::UnknownChecking);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::Watch { .. })]
        ));
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Waiting
        );

        let epoch = client.state().status.watch_epoch;
        let update = client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch,
            status: SystemStatus { ready: true },
        }));
        assert_eq!(client.state().access.status, AccessStatus::Authenticated);
        assert!(matches!(
            update.effects(),
            [
                ClientEffect::Config(ConfigEffect::Watch { .. }),
                ClientEffect::Session(SessionEffect::Watch { .. }),
                ClientEffect::Session(SessionEffect::ListAgents { .. }),
            ]
        ));
        let timeline_update = client.maintain_watches();
        assert!(matches!(
            timeline_update.effects(),
            [ClientEffect::Session(SessionEffect::WatchTimeline { .. })]
        ));
    }

    #[test]
    fn waltr_logout_clears_protected_data_and_provider_statuses() {
        let mut client = Client::new();
        client.apply(ClientEvent::Config(ConfigEvent::Loaded(catalog(
            config_with_provider("anthropic", "etag-1"),
        ))));
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));
        client.apply(ClientEvent::Stream(StreamEvent::Chunk {
            stream_id: 1,
            text: "protected".to_owned(),
        }));

        let update = client.logout_waltr();

        assert_eq!(client.state().access.status, AccessStatus::UnknownChecking);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::Watch { .. })]
        ));
        assert!(client.state().config.current.is_none());
        assert!(client.state().sessions.current.is_empty());
        assert!(client.state().sessions.timelines.is_empty());
        assert!(client.state().operations.is_empty());
        assert!(client.state().streams.items.is_empty());
        assert!(client.state().auth.statuses.is_empty());
    }

    #[test]
    fn stale_login_attempt_events_and_pre_logout_status_events_are_ignored() {
        let mut client = Client::new();
        client.start_waltr_login();
        let first = client.state().access.active_attempt_id.clone().unwrap();
        client.start_waltr_login();
        let second = client.state().access.active_attempt_id.clone().unwrap();
        assert!(second > first);

        client.fail_waltr_login(first.clone(), "stale failure");
        client.cancel_waltr_login(first.clone());
        assert_eq!(
            client.state().access.active_attempt_id,
            Some(second.clone())
        );
        assert_eq!(client.state().access.status, AccessStatus::LoginInProgress);
        assert!(client.complete_waltr_login(first).is_empty());

        client.logout_waltr();
        assert!(client.complete_waltr_login(second).is_empty());
        assert_eq!(client.state().access.status, AccessStatus::UnknownChecking);

        client.start_waltr_login();
        let current = client.state().access.active_attempt_id.clone().unwrap();
        let update = client.complete_waltr_login(current);
        let [ClientEffect::Status(StatusEffect::Watch { epoch })] = update.effects() else {
            panic!("current login should start a status watch");
        };
        let epoch = *epoch;
        client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch: epoch.saturating_sub(1),
            status: SystemStatus { ready: true },
        }));
        assert_eq!(client.state().access.status, AccessStatus::UnknownChecking);
        assert!(client.state().status.current.is_none());

        client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch,
            status: SystemStatus { ready: true },
        }));
        assert_eq!(client.state().access.status, AccessStatus::Authenticated);
    }

    #[test]
    fn logout_reprobe_authenticates_or_requires_login_from_current_epoch() {
        let mut client = Client::new();
        let update = client.logout_waltr();
        let [ClientEffect::Status(StatusEffect::Watch { epoch })] = update.effects() else {
            panic!("logout should emit exactly one status probe");
        };

        client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch: *epoch,
            status: SystemStatus { ready: true },
        }));
        assert_eq!(client.state().access.status, AccessStatus::Authenticated);

        let update = client.logout_waltr();
        let [ClientEffect::Status(StatusEffect::Watch { epoch })] = update.effects() else {
            panic!("second logout should emit exactly one status probe");
        };
        client.apply(ClientEvent::Status(StatusEvent::WatchFailedForEpoch {
            epoch: *epoch,
            error: ClientError::waltr_login_required("login required"),
        }));
        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);
    }

    #[test]
    fn stale_access_generation_completion_is_ignored() {
        let mut client = Client::new();
        let generation = client.state().access_generation;
        client.logout_waltr();

        client.apply_for_access_generation(
            generation,
            ClientEvent::Config(ConfigEvent::Loaded(catalog(config_with_provider(
                "stale",
                "stale-etag",
            )))),
        );

        assert!(client.state().config.current.is_none());
    }

    #[test]
    fn stale_operation_401_does_not_expire_current_access() {
        let mut client = Client::new();
        let first = client.create_session("default");
        let [
            ClientEffect::Session(SessionEffect::Create {
                operation_key: first_key,
                ..
            }),
        ] = first.effects()
        else {
            panic!("first create should emit an effect");
        };
        let second = client.create_session("default");
        let [
            ClientEffect::Session(SessionEffect::Create {
                operation_key: second_key,
                ..
            }),
        ] = second.effects()
        else {
            panic!("second create should emit an effect");
        };

        client.apply(ClientEvent::Session(SessionEvent::CreateFailed {
            operation_key: first_key.clone(),
            error: ClientError::waltr_login_required("stale unauthorized"),
        }));
        assert_eq!(client.state().access.status, AccessStatus::UnknownChecking);

        client.apply(ClientEvent::Session(SessionEvent::CreateFailed {
            operation_key: second_key.clone(),
            error: ClientError::waltr_login_required("current unauthorized"),
        }));
        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);
    }

    #[test]
    fn duplicate_expiry_does_not_replace_newer_login_attempt() {
        let mut client = Client::new();
        client.expire_waltr_access("expired");
        client.start_waltr_login();
        let attempt_id = client.state().access.active_attempt_id.clone();
        let generation = client.state().access_generation;

        client.expire_waltr_access("stale refresh failure");

        assert_eq!(client.state().access.status, AccessStatus::LoginInProgress);
        assert_eq!(client.state().access.active_attempt_id, attempt_id);
        assert_eq!(client.state().access_generation, generation);
    }

    #[test]
    fn expiry_preserves_watch_interests_but_logout_removes_them() {
        let mut client = Client::new();
        drop(client.register_session_watch_interest());
        drop(client.register_session_timeline_watch_interest(
            "session-1",
            Some(7),
            vec!["assistant_text".to_owned()],
            Some(10),
        ));

        client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::waltr_login_required("expired"),
        )));
        assert_eq!(client.state().sessions.watch.interests, 1);
        assert_eq!(client.state().timeline_watch_keys().len(), 1);

        client.logout_waltr();
        assert_eq!(client.state().sessions.watch.interests, 0);
        assert!(client.state().timeline_watch_keys().is_empty());
    }

    #[test]
    fn proactive_access_expiry_uses_transient_preservation_policy() {
        let mut client = Client::new();
        drop(client.register_config_watch_interest());
        drop(client.register_session_watch_interest());
        client.create_session("default");
        let operation_key = client.state().sessions.create.key.clone().unwrap();
        let update = client.expire_waltr_access("native token expired");

        assert!(update.is_empty());
        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);
        assert_eq!(
            client.state().access.error.as_deref(),
            Some("native token expired")
        );
        assert!(client.state().access.active_attempt_id.is_none());
        assert_eq!(client.state().config.watch.interests, 1);
        assert_eq!(client.state().sessions.watch.interests, 1);
        assert!(matches!(
            client.state().operations.get(&operation_key),
            Some(crate::ClientOperationState::SessionCreate {
                status: OperationStatus::Failed,
                ..
            })
        ));
        assert!(client.maintain_watches().is_empty());
    }

    #[test]
    fn access_loss_terminalizes_loading_operations_and_reloads_authoritative_data() {
        let mut client = Client::new();
        client.apply(ClientEvent::Config(ConfigEvent::Loaded(catalog(
            config_with_provider("anthropic", "etag-1"),
        ))));
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));
        let started = client.create_session("default");
        let operation_key = client.state().sessions.create.key.clone().unwrap();
        assert!(!started.is_empty());

        client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::waltr_login_required("expired"),
        )));
        assert!(matches!(
            client.state().operations.get(&operation_key),
            Some(crate::ClientOperationState::SessionCreate {
                status: OperationStatus::Failed,
                error: Some(error),
                session: None,
            }) if error == "Waltr login required"
        ));
        assert!(client.state().config.current.is_none());
        assert!(client.state().sessions.current.is_empty());

        client.start_waltr_login();
        let attempt_id = client.state().access.active_attempt_id.clone().unwrap();
        let update = client.complete_waltr_login(attempt_id);
        let [ClientEffect::Status(StatusEffect::Watch { epoch })] = update.effects() else {
            panic!("login completion should start status first");
        };
        let update = client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch: *epoch,
            status: SystemStatus { ready: true },
        }));
        assert!(
            update
                .effects()
                .contains(&ClientEffect::Config(ConfigEffect::Load))
        );
        assert!(
            update
                .effects()
                .contains(&ClientEffect::Session(SessionEffect::List))
        );
    }

    #[test]
    fn logout_preserves_pending_operation_key_but_redacts_identity_data() {
        let mut client = Client::new();
        client.delete_session("identity-session");
        let operation_key = client.state().sessions.delete.key.clone().unwrap();

        client.logout_waltr();

        assert!(matches!(
            client.state().operations.get(&operation_key),
            Some(crate::ClientOperationState::SessionDelete {
                status: OperationStatus::Failed,
                error: Some(error),
                id,
            }) if error == "Waltr login required" && id.is_empty()
        ));
    }

    #[test]
    fn request_config_sets_loading_and_emits_load_effect() {
        let mut client = Client::new();

        let update = client.request_config();

        assert_eq!(client.state().config.load.status, OperationStatus::Loading);
        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::Load)]
        );
    }

    #[test]
    fn config_loaded_sets_ready_configuration() {
        let mut client = Client::new();
        let config = Configuration {
            etag: "etag-1".to_owned(),
            ..Configuration::default()
        };

        let update = client.apply(ClientEvent::Config(ConfigEvent::Loaded(catalog(config))));

        assert!(update.is_empty());
        assert_eq!(client.state().config.load.status, OperationStatus::Ready);
        assert_eq!(
            client
                .state()
                .config
                .current
                .as_ref()
                .map(|config| config.etag.as_str()),
            Some("etag-1")
        );
    }

    #[test]
    fn config_operations_track_independent_statuses() {
        let mut client = Client::new();
        let config = config_with_provider("anthropic", "etag-1");

        client.apply(ClientEvent::Config(ConfigEvent::Loaded(catalog(
            config.clone(),
        ))));
        client.apply(ClientEvent::Config(ConfigEvent::PatchRequested(
            "op-test".to_owned(),
        )));
        client.apply(ClientEvent::Config(ConfigEvent::PatchFailed {
            operation_key: "op-test".to_owned(),
            error: ClientError::new("patch failed"),
        }));

        assert_eq!(client.state().config.load.status, OperationStatus::Ready);
        assert_eq!(client.state().config.patch.status, OperationStatus::Failed);
        assert_eq!(
            client.state().config.patch.error.as_deref(),
            Some("patch failed")
        );
        assert_eq!(client.state().config.current.as_ref(), Some(&config));

        client.apply(ClientEvent::Config(ConfigEvent::WatchFailed(
            ClientError::new("watch failed"),
        )));

        assert_eq!(client.state().config.watch.status, OperationStatus::Idle);
        assert_eq!(client.state().config.watch.error.as_deref(), None);
        assert_eq!(client.state().config.current.as_ref(), Some(&config));
    }

    #[test]
    fn config_reset_uses_current_etag_and_ignores_stale_completion() {
        let mut client = Client::new();
        let current = config_with_provider("custom", "etag-1");
        client.apply(ClientEvent::Config(ConfigEvent::Loaded(catalog(
            current.clone(),
        ))));

        let first = client.reset_config().expect("reset should start");
        let (first_key, _) = only_reset_config(&first);
        let first_key = first_key.to_owned();

        let second = client.reset_config().expect("newer reset should start");
        let (second_key, _) = only_reset_config(&second);
        let second_key = second_key.to_owned();
        assert_ne!(first_key, second_key);

        client.apply(ClientEvent::Config(ConfigEvent::Reset {
            operation_key: first_key,
        }));

        assert_eq!(client.state().config.current.as_ref(), Some(&current));
        assert_eq!(client.state().config.reset.status, OperationStatus::Loading);
        assert_eq!(client.state().config.reset.key.as_ref(), Some(&second_key));

        client.apply(ClientEvent::Config(ConfigEvent::Reset {
            operation_key: second_key,
        }));
        assert_eq!(client.state().config.current.as_ref(), Some(&current));
        assert_eq!(client.state().config.reset.status, OperationStatus::Ready);
    }

    #[test]
    fn config_reset_rejects_before_config_snapshot() {
        let mut client = Client::new();

        let error = client
            .reset_config()
            .expect_err("reset needs a config snapshot");

        assert_eq!(error.message(), "configuration not loaded");
        assert_eq!(client.state().config.reset.status, OperationStatus::Idle);
    }

    #[test]
    fn watch_events_update_config_or_watch_error() {
        let mut client = Client::new();
        let config = config_with_provider("anthropic", "etag-1");

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));
        assert_eq!(client.state().config.watch.status, OperationStatus::Loading);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Config(ConfigEffect::Watch { .. })]
        ));

        client.apply(ClientEvent::Config(ConfigEvent::WatchStarted));
        assert_eq!(client.state().config.watch.status, OperationStatus::Opened);

        client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                effective_configuration: Some(config.clone()),
                etag: config.etag.clone(),
                ..Default::default()
            },
        )));
        assert_eq!(
            client.state().config.watch.status,
            OperationStatus::Received
        );
        assert_eq!(client.state().config.current.as_ref(), Some(&config));

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                reload_error: "stream broke".to_owned(),
                effective_configuration: Some(config.clone()),
                etag: config.etag.clone(),
                ..Default::default()
            },
        )));
        assert_eq!(client.state().config.watch.status, OperationStatus::Failed);
        assert_eq!(
            client.state().config.watch.error.as_deref(),
            Some("stream broke")
        );
        assert_eq!(client.state().config.current.as_ref(), Some(&config));
        assert!(update.is_empty());
    }

    #[test]
    fn terminal_watch_open_failures_clear_identity_and_retry_only_once() {
        let mut client = Client::new();
        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));

        let config = client.register_config_watch_interest();
        let [
            ClientEffect::Config(ConfigEffect::Watch {
                open_id: config_open_id,
            }),
        ] = config.effects()
        else {
            panic!("config watch should open");
        };
        let config_failure = ClientEvent::Config(ConfigEvent::WatchOpenFailed {
            open_id: *config_open_id,
            error: ClientError::new("config open failed"),
        });
        assert_eq!(client.apply(config_failure.clone()).effects().len(), 1);
        assert!(client.apply(config_failure).is_empty());
        assert!(client.state().config.watch.open_id.is_none());

        let sessions = client.register_session_watch_interest();
        let session_open_id = sessions
            .effects()
            .iter()
            .find_map(|effect| match effect {
                ClientEffect::Session(SessionEffect::Watch { open_id }) => Some(*open_id),
                _ => None,
            })
            .expect("session watch should open");
        let session_failure = ClientEvent::Session(SessionEvent::WatchOpenFailed {
            open_id: session_open_id,
            error: ClientError::new("session open failed"),
        });
        assert_eq!(client.apply(session_failure.clone()).effects().len(), 1);
        assert!(client.apply(session_failure).is_empty());
        assert!(client.state().sessions.watch.open_id.is_none());

        let timeline = client.register_session_timeline_watch_interest(
            "session",
            Some(1),
            vec!["assistant_text".to_owned()],
            Some(10),
        );
        let [
            ClientEffect::Session(SessionEffect::WatchTimeline {
                key,
                open_id: timeline_open_id,
            }),
        ] = timeline.effects()
        else {
            panic!("timeline watch should open");
        };
        let timeline_failure = ClientEvent::Session(SessionEvent::TimelineWatchOpenFailed {
            key: key.clone(),
            open_id: *timeline_open_id,
            error: ClientError::new("timeline open failed"),
        });
        assert_eq!(client.apply(timeline_failure.clone()).effects().len(), 1);
        assert!(client.apply(timeline_failure).is_empty());
        assert!(
            client
                .state()
                .sessions
                .timelines
                .get("session")
                .and_then(|timeline| timeline.watches.get(key))
                .is_some_and(|watch| watch.open_id.is_none())
        );
    }

    #[test]
    fn watch_open_identity_exhaustion_fails_closed_without_aliasing() {
        let mut client = Client::new();
        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        client.state.exhaust_watch_open_ids_for_test();

        assert!(client.register_config_watch_interest().is_empty());
        assert_eq!(client.state().config.watch.status, OperationStatus::Failed);
        assert_eq!(
            client.state().config.watch.error.as_deref(),
            Some("watch open identity exhausted")
        );
        assert!(matches!(
            client.register_session_watch_interest().effects(),
            [ClientEffect::Session(SessionEffect::ListAgents { .. })]
        ));
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Failed
        );
        assert!(
            client
                .register_session_timeline_watch_interest("session", None, Vec::new(), None)
                .is_empty()
        );
        let timeline = client.state().sessions.timelines.get("session").unwrap();
        assert_eq!(timeline.watch.status, OperationStatus::Failed);
        assert!(
            timeline
                .watches
                .values()
                .all(|watch| watch.open_id.is_none())
        );
    }

    #[test]
    fn watch_events_replace_catalog_snapshots_authoritatively() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            providers: providers([provider("anthropic")]),
            mcp_servers: mcp_servers([mcp_server("filesystem")]),
            inference_defaults: None,
            session_dictation: None,
        });

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                etag: "etag-2".to_owned(),
                effective_configuration: Some(config_with_provider("openai", "etag-2")),
                ..Default::default()
            },
        )));

        assert!(update.is_empty());
        let current = client
            .state()
            .config
            .current
            .as_ref()
            .expect("config should remain loaded");
        assert_eq!(current.etag, "etag-2");
        assert_eq!(current.providers, providers([provider("openai")]));
        assert!(current.mcp_servers.is_empty());

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                etag: "etag-3".to_owned(),
                effective_configuration: Some(Configuration {
                    etag: "etag-3".to_owned(),
                    mcp_servers: mcp_servers([mcp_server("github")]),
                    ..Configuration::default()
                }),
                ..Default::default()
            },
        )));

        assert!(update.is_empty());
        let current = client
            .state()
            .config
            .current
            .as_ref()
            .expect("config should remain loaded");
        assert_eq!(current.etag, "etag-3");
        assert!(current.providers.is_empty());
        assert_eq!(current.mcp_servers, mcp_servers([mcp_server("github")]));
    }

    #[test]
    fn watch_events_accept_complete_snapshot_without_refetch() {
        let mut client = client_with_config(config_with_provider("anthropic", "etag-1"));

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                etag: "etag-2".to_owned(),
                effective_configuration: Some(config_with_provider("openai", "etag-2")),
                ..Default::default()
            },
        )));

        assert_eq!(
            client.state().config.watch.status,
            OperationStatus::Received
        );
        assert_eq!(
            client.state().config.current.as_ref().unwrap().etag,
            "etag-2"
        );
        assert!(update.is_empty());
    }

    #[test]
    fn session_template_search_sets_loading_and_emits_search_effect() {
        let mut client = Client::new();

        let update = client.search_session_templates(
            "resource-search-1",
            "session-1",
            "src",
            8,
            vec![
                ClientSessionTemplateKind::File,
                ClientSessionTemplateKind::Tool,
            ],
            ClientSessionTemplateSearchTarget::Resources,
            Some(3),
        );

        let search = client
            .state()
            .sessions
            .template_searches
            .get("session-1")
            .expect("resource search state should exist");
        assert_eq!(search.status, OperationStatus::Loading);
        assert_eq!(search.request_id, "resource-search-1");
        assert_eq!(search.query, "src");
        assert_eq!(
            update.effects(),
            &[ClientEffect::Session(SessionEffect::SearchTemplates {
                request_id: "resource-search-1".to_owned(),
                session_id: "session-1".to_owned(),
                query: "src".to_owned(),
                limit: 8,
                kinds: vec![SessionTemplateKind::File, SessionTemplateKind::Tool],
                target: crate::SessionTemplateSearchTarget::Resources,
                cursor: Some(3),
            })]
        );
    }

    #[test]
    fn catalog_search_and_suggestion_transition_through_success_and_failure() {
        let mut client = Client::new();

        let search = client.search_catalog_models(
            "catalog-search-1",
            "sonnet",
            Some("anthropic".to_owned()),
            10,
        );
        assert_eq!(
            client.state().catalog.search.status,
            OperationStatus::Loading
        );
        assert_eq!(client.state().catalog.search.query, "sonnet");
        assert_eq!(
            search.effects(),
            &[ClientEffect::Catalog(CatalogEffect::SearchModels {
                request_id: "catalog-search-1".to_owned(),
                query: "sonnet".to_owned(),
                provider_id: Some("anthropic".to_owned()),
                limit: 10,
            })]
        );

        client.apply(ClientEvent::Catalog(CatalogEvent::ModelsSearched {
            request_id: "catalog-search-1".to_owned(),
            models: vec![catalog_model("anthropic", "claude-sonnet")],
            stale: true,
        }));
        assert_eq!(client.state().catalog.search.status, OperationStatus::Ready);
        assert!(client.state().catalog.search.stale);
        assert_eq!(client.state().catalog.search.models.len(), 1);

        let suggestions =
            client.suggest_catalog_matches("catalog-suggest-1", "claude", "sonnet", 5);
        assert_eq!(
            suggestions.effects(),
            &[ClientEffect::Catalog(CatalogEffect::SuggestMatches {
                request_id: "catalog-suggest-1".to_owned(),
                provider_key: "claude".to_owned(),
                model_key: "sonnet".to_owned(),
                limit: 5,
            })]
        );
        client.apply(ClientEvent::Catalog(CatalogEvent::MatchesSuggested {
            request_id: "catalog-suggest-1".to_owned(),
            matches: vec![catalog_match("anthropic", "claude-sonnet")],
            stale: false,
        }));
        assert_eq!(
            client.state().catalog.suggestions.status,
            OperationStatus::Ready
        );
        assert_eq!(client.state().catalog.suggestions.matches.len(), 1);

        client.suggest_catalog_matches("catalog-suggest-2", "claude", "opus", 5);
        client.apply(ClientEvent::Catalog(CatalogEvent::MatchesSuggestFailed {
            request_id: "catalog-suggest-2".to_owned(),
            error: ClientError::new("suggest failed"),
        }));
        assert_eq!(
            client.state().catalog.suggestions.status,
            OperationStatus::Failed
        );
        assert_eq!(
            client.state().catalog.suggestions.error.as_deref(),
            Some("suggest failed")
        );
    }

    #[test]
    fn catalog_ignores_stale_request_and_access_generation_results() {
        let mut client = Client::new();
        client.search_catalog_models("catalog-search-1", "old", None, 10);
        client.search_catalog_models("catalog-search-2", "new", None, 10);

        client.apply(ClientEvent::Catalog(CatalogEvent::ModelsSearched {
            request_id: "catalog-search-1".to_owned(),
            models: vec![catalog_model("old", "old")],
            stale: false,
        }));
        assert_eq!(
            client.state().catalog.search.status,
            OperationStatus::Loading
        );
        assert_eq!(client.state().catalog.search.request_id, "catalog-search-2");
        assert!(client.state().catalog.search.models.is_empty());

        let generation = client.state().access_generation;
        client.logout_waltr();
        client.apply_for_access_generation(
            generation,
            ClientEvent::Catalog(CatalogEvent::ModelsSearched {
                request_id: "catalog-search-2".to_owned(),
                models: vec![catalog_model("stale", "stale")],
                stale: false,
            }),
        );
        assert!(client.state().catalog.search.models.is_empty());
        assert_eq!(client.state().catalog.search.status, OperationStatus::Idle);
    }

    #[test]
    fn current_catalog_unauthenticated_failure_requires_waltr_login() {
        let mut client = Client::new();
        client.suggest_catalog_matches("catalog-suggest-1", "claude", "sonnet", 5);

        client.apply(ClientEvent::Catalog(CatalogEvent::MatchesSuggestFailed {
            request_id: "catalog-suggest-1".to_owned(),
            error: ClientError::waltr_login_required("login required"),
        }));

        assert_eq!(client.state().access.status, AccessStatus::LoginRequired);
        assert_eq!(
            client.state().access.error.as_deref(),
            Some("login required")
        );
        assert_eq!(
            client.state().catalog.suggestions.status,
            OperationStatus::Idle
        );
    }

    #[test]
    fn session_template_search_result_updates_matching_request() {
        let mut client = Client::new();
        client.search_session_templates(
            "resource-search-1",
            "session-1",
            "src",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );

        let update = client.apply(ClientEvent::Session(SessionEvent::TemplatesSearched {
            request_id: "resource-search-1".to_owned(),
            session_id: "session-1".to_owned(),
            options: vec![resource_option("file", "src/main.rs", "@src/main.rs")],
        }));

        assert!(update.is_empty());
        let search = client
            .state()
            .sessions
            .template_searches
            .get("session-1")
            .expect("resource search state should exist");
        assert_eq!(search.status, OperationStatus::Ready);
        assert_eq!(search.options.len(), 1);
        assert_eq!(search.options[0].label, "src/main.rs");
    }

    #[test]
    fn session_template_search_failure_updates_matching_request() {
        let mut client = Client::new();
        client.search_session_templates(
            "resource-search-1",
            "session-1",
            "src",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );

        client.apply(ClientEvent::Session(SessionEvent::TemplatesSearchFailed {
            request_id: "resource-search-1".to_owned(),
            session_id: "session-1".to_owned(),
            error: ClientError::new("search failed"),
        }));

        let search = client
            .state()
            .sessions
            .template_searches
            .get("session-1")
            .expect("resource search state should exist");
        assert_eq!(search.status, OperationStatus::Failed);
        assert_eq!(search.error.as_deref(), Some("search failed"));
        assert!(search.options.is_empty());
    }

    #[test]
    fn session_template_search_ignores_stale_response() {
        let mut client = Client::new();
        client.search_session_templates(
            "resource-search-1",
            "session-1",
            "src",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );
        client.search_session_templates(
            "resource-search-2",
            "session-1",
            "src/lib",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );

        client.apply(ClientEvent::Session(SessionEvent::TemplatesSearched {
            request_id: "resource-search-1".to_owned(),
            session_id: "session-1".to_owned(),
            options: vec![resource_option("file", "src/main.rs", "@src/main.rs")],
        }));

        let search = client
            .state()
            .sessions
            .template_searches
            .get("session-1")
            .expect("resource search state should exist");
        assert_eq!(search.status, OperationStatus::Loading);
        assert_eq!(search.request_id, "resource-search-2");
        assert!(search.options.is_empty());
    }

    #[test]
    fn config_watch_waits_when_status_is_not_ready() {
        let mut client = Client::new();

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));

        assert!(update.is_empty());
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);

        let epoch = client.state().status.watch_epoch;
        let update = client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch,
            status: SystemStatus { ready: true },
        }));

        assert!(matches!(
            update.effects(),
            [ClientEffect::Config(ConfigEffect::Watch { .. })]
        ));
        assert_eq!(client.state().config.watch.status, OperationStatus::Loading);
    }

    #[test]
    fn config_watch_failure_reconnects_config_watch_directly() {
        let mut client = Client::new();

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered)));

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchFailed(
            ClientError::new("lost config"),
        )));
        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::SleepReconnect {
                delay_ms: 500
            })]
        );
        assert_eq!(client.state().config.watch.status, OperationStatus::Failed);
        assert_eq!(
            client.state().status.watch.status,
            OperationStatus::Received
        );
        assert!(client.state().is_system_ready());

        let update = client.apply(ClientEvent::Config(ConfigEvent::ReconnectReady));
        assert!(matches!(
            update.effects(),
            [ClientEffect::Config(ConfigEffect::Watch { .. })]
        ));

        drop(client.apply(ClientEvent::Config(ConfigEvent::WatchStarted)));
        drop(client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate::default(),
        ))));
        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchFailed(
            ClientError::new("lost config"),
        )));
        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::SleepReconnect {
                delay_ms: 500
            })]
        );
    }

    #[test]
    fn config_watch_reconnect_waits_for_status_recovery() {
        let mut client = Client::new();

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered)));
        let status_update = client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::new("lost status"),
        )));
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);
        assert_eq!(
            status_update.effects(),
            &[ClientEffect::Status(StatusEffect::SleepReconnect {
                epoch: 0,
                delay_ms: 500
            })]
        );

        let config_update = client.apply(ClientEvent::Config(ConfigEvent::ReconnectReady));
        assert!(config_update.is_empty());
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);

        let recovery_update = client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        assert!(matches!(
            recovery_update.effects(),
            [ClientEffect::Config(ConfigEffect::Watch { .. })]
        ));
    }

    #[test]
    fn config_watch_release_stops_future_reconnects() {
        let mut client = Client::new();

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered)));
        drop(client.apply(ClientEvent::Config(ConfigEvent::WatchInterestReleased)));

        let update = client.apply(ClientEvent::Config(ConfigEvent::ReconnectReady));

        assert!(update.is_empty());
        assert_eq!(client.state().config.watch.status, OperationStatus::Idle);
    }

    #[test]
    fn session_watch_deletes_current_session_and_refetches_on_gap() {
        let mut client = Client::new();
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));

        let update = client.apply(ClientEvent::Session(SessionEvent::WatchChanged(
            SessionWatchUpdate {
                sequence: 2,
                event: SessionWatchEvent::Deleted("one".to_owned()),
            },
        )));

        assert!(!client.state().sessions.current.contains_key("one"));
        assert_eq!(client.state().sessions.last_sequence, 2);
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Received
        );
        assert_eq!(
            update.effects(),
            &[ClientEffect::Session(SessionEffect::List)]
        );
    }

    #[test]
    fn session_watch_snapshot_replaces_current_sessions() {
        let mut client = Client::new();
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "stale",
        )])));

        let update = client.apply(ClientEvent::Session(SessionEvent::WatchChanged(
            SessionWatchUpdate {
                sequence: 7,
                event: SessionWatchEvent::Snapshot(vec![session("fresh")]),
            },
        )));

        assert!(update.is_empty());
        assert!(!client.state().sessions.current.contains_key("stale"));
        assert!(client.state().sessions.current.contains_key("fresh"));
        assert_eq!(client.state().sessions.last_sequence, 7);
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Received
        );
    }

    #[test]
    fn session_timeline_page_loads_merge_by_cursor() {
        let mut client = Client::new();

        client.apply(ClientEvent::Session(SessionEvent::TimelineWatchChanged {
            key: crate::TimelineWatchKey::new("session".to_owned(), Some(0), Vec::new(), None),
            update: SessionTimelineWatchUpdate {
                sequence: 1,
                event: SessionTimelineWatchEvent::Event(timeline_event(10)),
            },
        }));
        complete_timeline_page(
            &mut client,
            "session",
            timeline_page(vec![timeline_event(8), timeline_event(9)]),
        );

        let timeline = client
            .state()
            .sessions
            .timelines
            .get("session")
            .expect("timeline exists");
        assert_eq!(
            timeline
                .entries
                .values()
                .filter_map(|entry| match entry.event.as_ref() {
                    Some(crate::projected_timeline_event::Event::Durable(event)) => {
                        Some(event.cursor)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![10, 8, 9]
        );
        assert_eq!(timeline.load.status, OperationStatus::Ready);
        assert_eq!(timeline.watch.status, OperationStatus::Received);
    }

    #[test]
    fn effective_timeline_projection_replaces_then_appends_and_dedupes() {
        let mut client = Client::new();
        let key = crate::TimelineWatchKey::new_with_projection(
            "session".to_owned(),
            None,
            Vec::new(),
            None,
            crate::SessionTimelineProjectionMode::Effective,
        );
        drop(
            client.register_session_timeline_watch_interest_with_projection(
                "session",
                None,
                Vec::new(),
                None,
                crate::SessionTimelineProjectionMode::Effective,
            ),
        );
        client.apply(ClientEvent::Session(SessionEvent::TimelineWatchChanged {
            key: key.clone(),
            update: SessionTimelineWatchUpdate {
                sequence: 1,
                event: SessionTimelineWatchEvent::ProjectionReset {
                    entries: vec![synthetic_timeline_event("mask-1", 5)],
                    projection: Some(crate::TimelineProjectionMetadata {
                        revision: 7,
                        raw_page_boundary: Some(3),
                        has_more: true,
                    }),
                },
            },
        }));
        for sequence in [2, 3] {
            client.apply(ClientEvent::Session(SessionEvent::TimelineWatchChanged {
                key: key.clone(),
                update: SessionTimelineWatchUpdate {
                    sequence,
                    event: SessionTimelineWatchEvent::ProjectedEvent(durable_projection(
                        timeline_event(6),
                    )),
                },
            }));
        }

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.entries.len(), 2);
        assert_eq!(timeline.projection_revision, Some(7));
        assert_eq!(timeline.raw_page_boundary, Some(3));
        assert!(timeline.has_more);

        client.apply(ClientEvent::Session(SessionEvent::TimelineWatchChanged {
            key,
            update: SessionTimelineWatchUpdate {
                sequence: 4,
                event: SessionTimelineWatchEvent::ProjectionReset {
                    entries: vec![synthetic_timeline_event("mask-2", 8)],
                    projection: Some(crate::TimelineProjectionMetadata {
                        revision: 8,
                        raw_page_boundary: None,
                        has_more: false,
                    }),
                },
            },
        }));
        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.entries.len(), 1);
        assert!(timeline.entries.contains_key("mask:mask-2"));
        assert_eq!(timeline.projection_revision, Some(8));
        assert_eq!(timeline.raw_page_boundary, None);
        assert!(!timeline.has_more);
    }

    #[test]
    fn timeline_effects_preserve_raw_and_allow_explicit_effective_mode() {
        let mut client = Client::new();
        let raw = client.request_session_timeline("session", None);
        assert!(matches!(
            raw.effects(),
            [ClientEffect::Session(SessionEffect::GetTimeline {
                projection_mode: crate::SessionTimelineProjectionMode::Raw,
                ..
            })]
        ));
        let effective = client.request_session_timeline_with_projection(
            "session",
            None,
            crate::SessionTimelineProjectionMode::Effective,
        );
        assert!(matches!(
            effective.effects(),
            [ClientEffect::Session(SessionEffect::GetTimeline {
                projection_mode: crate::SessionTimelineProjectionMode::Effective,
                ..
            })]
        ));
    }

    #[test]
    fn effective_reset_before_page_completion_discards_stale_page() {
        let mut client = Client::new();
        apply_projection_reset(&mut client, 5, "mask-5", 5);
        let request = begin_effective_page(&mut client);
        apply_projection_reset(&mut client, 6, "mask-6", 6);

        apply_effective_page(&mut client, request, 5, Some(2), 4);

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.projection_revision, Some(6));
        assert_eq!(timeline.raw_page_boundary, None);
        assert!(!timeline.has_more);
        assert_eq!(timeline.entries.len(), 1);
        assert!(timeline.entries.contains_key("mask:mask-6"));
    }

    #[test]
    fn effective_page_before_reset_is_replaced_by_authoritative_reset() {
        let mut client = Client::new();
        apply_projection_reset(&mut client, 5, "mask-5", 5);
        let request = begin_effective_page(&mut client);

        apply_effective_page(&mut client, request, 5, Some(2), 4);
        assert!(
            client.state().sessions.timelines["session"]
                .entries
                .contains_key("durable:4:i4")
        );
        apply_projection_reset(&mut client, 6, "mask-6", 6);

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.projection_revision, Some(6));
        assert_eq!(timeline.entries.len(), 1);
        assert!(timeline.entries.contains_key("mask:mask-6"));
    }

    #[test]
    fn overlapping_timeline_page_responses_accept_only_latest_request() {
        let mut client = Client::new();
        apply_projection_reset(&mut client, 5, "mask-5", 5);
        let older = begin_effective_page(&mut client);
        let latest = begin_effective_page(&mut client);

        apply_effective_page(&mut client, older, 5, Some(3), 3);
        assert_eq!(
            client.state().sessions.timelines["session"].entries.len(),
            1
        );
        assert_eq!(
            client.state().sessions.timelines["session"].load.status,
            OperationStatus::Loading
        );
        apply_effective_page(&mut client, latest, 5, Some(2), 4);

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.entries.len(), 2);
        assert!(timeline.entries.contains_key("durable:4:i4"));
        assert_eq!(timeline.raw_page_boundary, Some(2));
    }

    #[test]
    fn effective_page_completion_never_regresses_projection_revision() {
        let mut client = Client::new();
        apply_projection_reset(&mut client, 5, "mask-5", 5);
        let request = begin_effective_page(&mut client);

        apply_effective_page(&mut client, request, 4, Some(1), 3);

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.projection_revision, Some(5));
        assert_eq!(timeline.raw_page_boundary, None);
        assert_eq!(timeline.entries.len(), 1);
        assert!(!timeline.entries.contains_key("durable:3:i3"));
    }

    #[test]
    fn reset_before_old_timeline_failure_preserves_authoritative_state() {
        let mut client = Client::new();
        apply_projection_reset(&mut client, 5, "mask-5", 5);
        let request = begin_effective_page(&mut client);
        apply_projection_reset(&mut client, 6, "mask-6", 6);

        apply_timeline_failure(&mut client, request, "old request failed");

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.load.status, OperationStatus::Ready);
        assert_eq!(timeline.load.error, None);
        assert_eq!(timeline.load_request_id, None);
        assert_eq!(timeline.projection_revision, Some(6));
        assert_eq!(timeline.raw_page_boundary, None);
        assert!(!timeline.has_more);
        assert_eq!(timeline.entries.len(), 1);
        assert!(timeline.entries.contains_key("mask:mask-6"));
    }

    #[test]
    fn superseding_timeline_request_ignores_old_failure() {
        let mut client = Client::new();
        apply_projection_reset(&mut client, 5, "mask-5", 5);
        let old = begin_effective_page(&mut client);
        let current = begin_effective_page(&mut client);

        apply_timeline_failure(&mut client, old, "old request failed");

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.load.status, OperationStatus::Loading);
        assert_eq!(timeline.load.error, None);
        assert_eq!(timeline.load_request_id, Some(current.0));
        assert_eq!(timeline.projection_revision, Some(5));
        assert_eq!(timeline.entries.len(), 1);
    }

    #[test]
    fn current_timeline_failure_sets_failed_operation_state() {
        let mut client = Client::new();
        apply_projection_reset(&mut client, 5, "mask-5", 5);
        let request = begin_effective_page(&mut client);

        apply_timeline_failure(&mut client, request, "current request failed");

        let timeline = &client.state().sessions.timelines["session"];
        assert_eq!(timeline.load.status, OperationStatus::Failed);
        assert_eq!(
            timeline.load.error.as_deref(),
            Some("current request failed")
        );
        assert_eq!(timeline.load_request_id, None);
        assert_eq!(timeline.projection_revision, Some(5));
        assert_eq!(timeline.entries.len(), 1);
    }

    #[test]
    fn continue_session_from_cursor_emits_cursor_target() {
        let mut client = Client::new();

        let update = client.continue_session_from_cursor("session", 42);

        assert_eq!(
            client.state().sessions.continue_session.status,
            OperationStatus::Loading
        );
        assert_eq!(update.effects().len(), 1);
        match &update.effects()[0] {
            ClientEffect::Session(SessionEffect::Continue {
                session_id, target, ..
            }) => {
                assert_eq!(session_id, "session");
                assert_eq!(
                    target.target,
                    Some(crate::rollback_target::Target::Cursor(42))
                );
            }
            effect => panic!("unexpected effect: {effect:?}"),
        }
    }

    #[test]
    fn rollback_session_before_cursor_emits_before_cursor_target() {
        let mut client = Client::new();

        let update = client.rollback_session_before_cursor("session", 42);

        assert_eq!(
            client.state().sessions.rollback.status,
            OperationStatus::Loading
        );
        assert_eq!(update.effects().len(), 1);
        match &update.effects()[0] {
            ClientEffect::Session(SessionEffect::Rollback {
                session_id, target, ..
            }) => {
                assert_eq!(session_id, "session");
                assert_eq!(
                    target.target,
                    Some(crate::rollback_target::Target::BeforeCursor(42))
                );
            }
            effect => panic!("unexpected effect: {effect:?}"),
        }
    }

    #[test]
    fn session_create_completion_only_updates_operation_result() {
        let mut client = Client::new();
        let operation_key = OperationKey::from("create-session");

        client.state.sessions.create.key = Some(operation_key.clone());

        client.apply(ClientEvent::Session(SessionEvent::Created {
            operation_key,
            session: session("one"),
        }));

        assert!(client.state().sessions.current.is_empty());
        assert_eq!(
            client.state().sessions.create.status,
            OperationStatus::Ready
        );
    }

    #[test]
    fn session_delete_completion_only_updates_operation_result() {
        let mut client = Client::new();
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));
        let operation_key = OperationKey::from("delete-session");

        client.apply(ClientEvent::Session(SessionEvent::Deleted {
            operation_key,
            id: "one".to_owned(),
        }));

        assert!(client.state().sessions.current.contains_key("one"));
        assert_eq!(
            client.state().sessions.delete.status,
            OperationStatus::Ready
        );
    }

    #[test]
    fn session_delete_completion_preserves_authoritative_session_caches() {
        let mut client = Client::new();
        client.search_session_templates(
            "search-one",
            "one",
            "src",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );

        client.apply(ClientEvent::Session(SessionEvent::Deleted {
            operation_key: "delete-session".to_owned(),
            id: "one".to_owned(),
        }));

        assert!(
            client
                .state()
                .sessions
                .template_searches
                .contains_key("one")
        );
    }

    #[test]
    fn session_wipe_is_keyed_and_ignores_stale_success() {
        let mut client = Client::new();
        let first = client.wipe_sessions();
        let [
            ClientEffect::Session(SessionEffect::Wipe {
                operation_key: first_key,
            }),
        ] = first.effects()
        else {
            panic!("session wipe effect");
        };
        let first_key = first_key.clone();
        let second = client.wipe_sessions();
        let [
            ClientEffect::Session(SessionEffect::Wipe {
                operation_key: second_key,
            }),
        ] = second.effects()
        else {
            panic!("second session wipe effect");
        };
        let second_key = second_key.clone();
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));

        client.apply(ClientEvent::Session(SessionEvent::Wiped {
            operation_key: first_key,
        }));

        assert!(client.state().sessions.current.contains_key("one"));
        assert_eq!(
            client.state().sessions.wipe.status,
            OperationStatus::Loading
        );
        assert_eq!(client.state().sessions.wipe.key.as_ref(), Some(&second_key));
    }

    #[test]
    fn matching_session_wipe_waits_for_authoritative_watch_clear() {
        let mut client = Client::new();
        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.register_session_watch_interest());
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));
        complete_timeline_page(&mut client, "one", timeline_page(vec![timeline_event(1)]));
        client.search_session_templates(
            "search-one",
            "one",
            "src",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );
        drop(client.register_session_timeline_watch_interest("one", None, Vec::new(), None));
        let wipe = client.wipe_sessions();
        let [ClientEffect::Session(SessionEffect::Wipe { operation_key })] = wipe.effects() else {
            panic!("session wipe effect");
        };
        let operation_key = operation_key.clone();

        client.apply(ClientEvent::Session(SessionEvent::Wiped {
            operation_key: operation_key.clone(),
        }));

        assert!(client.state().sessions.current.contains_key("one"));
        assert!(client.state().sessions.timelines.contains_key("one"));
        assert!(
            client
                .state()
                .sessions
                .template_searches
                .contains_key("one")
        );
        assert_eq!(client.state().sessions.watch.interests, 1);
        assert_eq!(client.state().sessions.wipe.status, OperationStatus::Ready);
        assert!(matches!(
            client.state().operations.get(&operation_key),
            Some(crate::ClientOperationState::SessionWipe {
                status: OperationStatus::Ready,
                error: None,
            })
        ));
    }

    #[test]
    fn session_wipe_failure_retains_session_state() {
        let mut client = Client::new();
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));
        complete_timeline_page(&mut client, "one", timeline_page(vec![timeline_event(1)]));
        client.search_session_templates(
            "search-one",
            "one",
            "src",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );
        let wipe = client.wipe_sessions();
        let [ClientEffect::Session(SessionEffect::Wipe { operation_key })] = wipe.effects() else {
            panic!("session wipe effect");
        };
        let operation_key = operation_key.clone();

        client.apply(ClientEvent::Session(SessionEvent::WipeFailed {
            operation_key: operation_key.clone(),
            error: ClientError::new("wipe failed"),
        }));

        assert!(client.state().sessions.current.contains_key("one"));
        assert!(client.state().sessions.timelines.contains_key("one"));
        assert!(
            client
                .state()
                .sessions
                .template_searches
                .contains_key("one")
        );
        assert_eq!(client.state().sessions.wipe.status, OperationStatus::Failed);
        assert!(matches!(
            client.state().operations.get(&operation_key),
            Some(crate::ClientOperationState::SessionWipe {
                status: OperationStatus::Failed,
                error: Some(error),
            }) if error == "wipe failed"
        ));
    }

    #[test]
    fn remote_session_clear_clears_all_session_caches_without_changing_wipe_result() {
        let mut client = Client::new();
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));
        complete_timeline_page(&mut client, "one", timeline_page(vec![timeline_event(1)]));
        client.search_session_templates(
            "search-one",
            "one",
            "src",
            8,
            vec![ClientSessionTemplateKind::File],
            ClientSessionTemplateSearchTarget::Resources,
            None,
        );
        drop(client.request_sessions());
        assert_eq!(
            client.state().sessions.list.status,
            OperationStatus::Loading
        );

        client.apply(ClientEvent::Session(SessionEvent::WatchChanged(
            SessionWatchUpdate {
                sequence: 1,
                event: SessionWatchEvent::Cleared,
            },
        )));

        assert!(client.state().sessions.current.is_empty());
        assert!(client.state().sessions.timelines.is_empty());
        assert!(client.state().sessions.template_searches.is_empty());
        assert_eq!(client.state().sessions.list.status, OperationStatus::Ready);
        assert_eq!(client.state().sessions.wipe.status, OperationStatus::Idle);
        assert_eq!(client.state().sessions.last_sequence, 1);

        let future_list = client.request_sessions();
        assert_eq!(
            future_list.effects(),
            &[ClientEffect::Session(SessionEffect::List)]
        );
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "future",
        )])));
        assert!(client.state().sessions.current.contains_key("future"));
        assert_eq!(client.state().sessions.list.status, OperationStatus::Ready);
    }

    #[test]
    fn session_watch_failure_reconnects_session_watch_directly() {
        let mut client = Client::new();

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.apply(ClientEvent::Session(SessionEvent::WatchInterestRegistered)));
        drop(client.apply(ClientEvent::Session(SessionEvent::WatchStarted)));

        let update = client.apply(ClientEvent::Session(SessionEvent::WatchFailed(
            ClientError::new("lost session watch"),
        )));

        assert_eq!(
            client.state().status.watch.status,
            OperationStatus::Received
        );
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Failed
        );
        assert_eq!(
            update.effects(),
            &[ClientEffect::Session(SessionEffect::SleepReconnect {
                delay_ms: 500
            })]
        );
    }

    #[test]
    fn status_recovery_maintains_waiting_session_watch() {
        let mut client = Client::new();

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.apply(ClientEvent::Session(SessionEvent::WatchInterestRegistered)));

        let status_update = client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::new("lost status"),
        )));
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Waiting
        );
        assert_eq!(
            status_update.effects(),
            &[ClientEffect::Status(StatusEffect::SleepReconnect {
                epoch: 0,
                delay_ms: 500
            })]
        );

        let recovery_update = client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));

        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Loading
        );
        assert!(matches!(
            recovery_update.effects(),
            [
                ClientEffect::Session(SessionEffect::Watch { .. }),
                ClientEffect::Session(SessionEffect::ListAgents { .. })
            ]
        ));
    }

    #[test]
    fn session_watch_reconnect_waits_for_status_recovery() {
        let mut client = Client::new();

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.apply(ClientEvent::Session(SessionEvent::WatchInterestRegistered)));
        drop(client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::new("lost status"),
        ))));

        let session_update = client.apply(ClientEvent::Session(SessionEvent::ReconnectReady));
        assert!(session_update.is_empty());
        assert_eq!(
            client.state().sessions.watch.status,
            OperationStatus::Waiting
        );
    }

    #[test]
    fn status_failure_blocks_config_reconnects() {
        let mut client = Client::new();

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        drop(client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered)));

        let status_update = client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::new("lost"),
        )));
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);
        assert_eq!(
            status_update.effects(),
            &[ClientEffect::Status(StatusEffect::SleepReconnect {
                epoch: 0,
                delay_ms: 500
            })]
        );

        let config_update = client.apply(ClientEvent::Config(ConfigEvent::ReconnectReady));
        assert!(config_update.is_empty());
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);
    }

    #[test]
    fn status_watch_updates_readiness_and_reconnects_with_capped_backoff() {
        let mut client = Client::new();

        let update = client.request_status_watch();
        assert_eq!(client.state().status.watch.status, OperationStatus::Loading);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::Watch { .. })]
        ));

        let epoch = client.state().status.watch_epoch;
        let update = client.apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
            epoch,
            status: SystemStatus { ready: true },
        }));
        assert!(update.is_empty());
        assert_eq!(
            client.state().status.watch.status,
            OperationStatus::Received
        );
        assert!(
            client
                .state()
                .status
                .current
                .is_some_and(|status| status.ready)
        );

        let update = client.apply(ClientEvent::Status(StatusEvent::WatchFailedForEpoch {
            epoch,
            error: ClientError::new("lost status"),
        }));
        assert_eq!(client.state().status.watch.status, OperationStatus::Failed);
        assert!(!client.state().is_system_ready());
        assert_eq!(
            client.state().status.watch.error.as_deref(),
            Some("lost status")
        );
        assert_eq!(
            update.effects(),
            &[ClientEffect::Status(StatusEffect::SleepReconnect {
                epoch: 1,
                delay_ms: 500
            })]
        );

        for expected_delay in [1_000, 2_000, 5_000, 5_000] {
            let update = client.apply(ClientEvent::Status(StatusEvent::WatchFailedForEpoch {
                epoch,
                error: ClientError::new("lost status"),
            }));
            assert_eq!(
                update.effects(),
                &[ClientEffect::Status(StatusEffect::SleepReconnect {
                    epoch: 1,
                    delay_ms: expected_delay
                })]
            );
        }

        let update = client.apply(ClientEvent::Status(StatusEvent::ReconnectReadyForEpoch {
            epoch,
        }));
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::Watch { .. })]
        ));
        assert_eq!(watch_reconnect_delay_ms(99), 5_000);
    }

    #[test]
    fn provider_commands_emit_canonical_resource_writes() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            providers: providers([provider("anthropic")]),
            ..Configuration::default()
        });

        let update = client
            .add_provider(provider("openai"))
            .expect("add provider");
        assert_eq!(
            provider_write_paths(only_resource_writes(&update)),
            vec!["providers/openai.yaml"]
        );

        let update = client
            .update_provider(
                "anthropic",
                provider("anthropic"),
                provider("claude"),
                &edit_baseline("providers/anthropic.yaml"),
            )
            .expect("update provider");
        assert_eq!(
            resource_write_paths(only_resource_writes(&update)),
            vec!["providers/claude.yaml", "providers/anthropic.yaml"]
        );

        let update = client
            .rename_provider("anthropic", "renamed")
            .expect("rename provider");
        assert_eq!(
            resource_write_paths(only_resource_writes(&update)),
            vec!["providers/renamed.yaml", "providers/anthropic.yaml"]
        );

        let update = client
            .delete_provider("anthropic")
            .expect("delete provider");
        assert_eq!(
            resource_write_paths(only_resource_writes(&update)),
            vec!["providers/anthropic.yaml"]
        );
    }

    #[test]
    fn provider_patch_helpers_reject_before_config_snapshot() {
        let mut client = Client::new();

        let error = client
            .add_provider(provider("anthropic"))
            .expect_err("provider patches need a config snapshot");

        assert_eq!(error.message(), "configuration not loaded");
        assert_eq!(client.state().config.patch.status, OperationStatus::Idle);
    }

    #[test]
    fn provider_validation_rejects_empty_and_duplicate_names() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            providers: providers([provider("anthropic")]),
            ..Configuration::default()
        });

        let empty_error = client
            .add_provider(provider(""))
            .expect_err("empty provider names should fail");
        assert!(empty_error.message().contains("cannot be empty"));

        let space_error = client
            .add_provider(provider("open ai"))
            .expect_err("provider names with spaces should fail");
        assert!(space_error.message().contains("cannot contain spaces"));

        let uppercase_error = client
            .add_provider(provider("OpenAI"))
            .expect_err("uppercase provider names should fail");
        assert!(uppercase_error.message().contains("must be lowercase"));

        let duplicate_error = client
            .add_provider(provider("anthropic"))
            .expect_err("duplicate provider names should fail");
        assert!(duplicate_error.message().contains("already used"));

        let rename_error = client
            .rename_provider("anthropic", "")
            .expect_err("empty rename should fail");
        assert!(rename_error.message().contains("cannot be empty"));
    }

    #[test]
    fn mcp_server_commands_emit_canonical_resource_writes() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            mcp_servers: mcp_servers([mcp_server("filesystem")]),
            ..Configuration::default()
        });

        let update = client
            .add_mcp_server(mcp_server("github"))
            .expect("add mcp server");
        assert_eq!(
            resource_write_paths(only_resource_writes(&update)),
            vec!["mcp-servers/github.yaml"]
        );

        let update = client
            .update_mcp_server(
                "filesystem",
                mcp_server("filesystem"),
                mcp_server("fs"),
                &edit_baseline("mcp-servers/filesystem.yaml"),
            )
            .expect("update mcp server");
        assert_eq!(
            resource_write_paths(only_resource_writes(&update)),
            vec!["mcp-servers/fs.yaml", "mcp-servers/filesystem.yaml"]
        );

        let update = client
            .delete_mcp_server("filesystem")
            .expect("delete mcp server");
        assert_eq!(
            resource_write_paths(only_resource_writes(&update)),
            vec!["mcp-servers/filesystem.yaml"]
        );
    }

    #[test]
    fn mcp_server_validation_rejects_empty_and_duplicate_keys() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            mcp_servers: mcp_servers([mcp_server("filesystem")]),
            ..Configuration::default()
        });

        let empty_error = client
            .add_mcp_server(mcp_server(""))
            .expect_err("empty MCP server keys should fail");
        assert!(empty_error.message().contains("cannot be empty"));

        let space_error = client
            .add_mcp_server(mcp_server("file system"))
            .expect_err("MCP server keys with spaces should fail");
        assert!(space_error.message().contains("cannot contain spaces"));

        let uppercase_error = client
            .add_mcp_server(mcp_server("FileSystem"))
            .expect_err("uppercase MCP server keys should fail");
        assert!(uppercase_error.message().contains("must be lowercase"));

        let duplicate_error = client
            .add_mcp_server(mcp_server("filesystem"))
            .expect_err("duplicate MCP server keys should fail");
        assert!(duplicate_error.message().contains("already used"));

        let update_error = client
            .update_mcp_server(
                "filesystem",
                mcp_server("filesystem"),
                mcp_server(""),
                &edit_baseline("mcp-servers/filesystem.yaml"),
            )
            .expect_err("empty update key should fail");
        assert!(update_error.message().contains("cannot be empty"));
    }

    #[test]
    fn provider_conflict_detection_compares_provider_snapshot() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            providers: providers([provider("anthropic"), provider("openai")]),
            ..Configuration::default()
        });
        let baseline = client
            .provider_edit_baseline("anthropic")
            .expect("baseline should exist");

        client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                effective_configuration: Some(Configuration {
                    etag: "etag-2".to_owned(),
                    providers: providers([provider("anthropic"), provider("new-provider")]),
                    ..Configuration::default()
                }),
                ..ConfigWatchUpdate::default()
            },
        )));

        assert_eq!(client.provider_changed_since(&baseline), Ok(false));

        client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                effective_configuration: Some(Configuration {
                    etag: "etag-3".to_owned(),
                    providers: providers([provider("changed"), provider("new-provider")]),
                    ..Configuration::default()
                }),
                ..ConfigWatchUpdate::default()
            },
        )));

        assert_eq!(client.provider_changed_since(&baseline), Ok(true));
    }

    #[test]
    fn stream_events_accumulate_text_and_status() {
        let mut client = Client::new();

        client.apply(ClientEvent::Stream(StreamEvent::Chunk {
            stream_id: 7,
            text: "hel".to_owned(),
        }));
        client.apply(ClientEvent::Stream(StreamEvent::Chunk {
            stream_id: 7,
            text: "lo".to_owned(),
        }));
        client.apply(ClientEvent::Stream(StreamEvent::Finished { stream_id: 7 }));

        let stream = client
            .state()
            .streams
            .items
            .get(&7)
            .expect("stream should exist");
        assert_eq!(stream.text(), "hello");
        assert_eq!(stream.status(), StreamStatus::Finished);
    }

    fn client_with_config(config: Configuration) -> Client {
        let mut client = Client::new();
        client.apply(ClientEvent::Config(ConfigEvent::Loaded(catalog(config))));
        client
    }

    fn catalog(config: Configuration) -> crate::ResourceCatalog {
        let mut resources = config
            .providers
            .values()
            .map(|provider| crate::Resource {
                path: provider_path(&provider.name),
                kind: ResourceKind::Provider as i32,
                name: provider.name.clone(),
                origin: ResourceOrigin::Bundled as i32,
                persisted_etag: "absent-etag".to_owned(),
                active_source: crate::resource::provider_source(provider, None, None).unwrap(),
                ..crate::Resource::default()
            })
            .chain(config.mcp_servers.values().map(|server| crate::Resource {
                path: mcp_server_path(&server.name),
                kind: ResourceKind::McpServer as i32,
                name: server.name.clone(),
                origin: ResourceOrigin::Bundled as i32,
                persisted_etag: "absent-etag".to_owned(),
                active_source: crate::resource::mcp_server_source(server, None, None).unwrap(),
                ..crate::Resource::default()
            }))
            .collect::<Vec<_>>();
        if let Some(defaults) = config.inference_defaults.as_ref() {
            resources.push(crate::Resource {
                path: "defaults/default.yaml".to_owned(),
                kind: ResourceKind::Defaults as i32,
                name: "default".to_owned(),
                origin: ResourceOrigin::Bundled as i32,
                persisted_etag: "absent-etag".to_owned(),
                active_source: crate::resource::defaults_source(defaults, None, None).unwrap(),
                ..crate::Resource::default()
            });
        }
        crate::ResourceCatalog {
            resources,
            etag: config.etag.clone(),
            effective_configuration: Some(config),
            ..Default::default()
        }
    }

    fn config_with_provider(name: &str, etag: &str) -> Configuration {
        Configuration {
            etag: etag.to_owned(),
            providers: providers([provider(name)]),
            ..Default::default()
        }
    }

    fn providers<const N: usize>(
        providers: [Provider; N],
    ) -> std::collections::HashMap<String, Provider> {
        providers
            .into_iter()
            .map(|provider| (provider.name.clone(), provider))
            .collect()
    }

    fn provider(name: &str) -> Provider {
        Provider {
            name: name.to_owned(),
            r#type: crate::ProviderType::AnthropicMessages as i32,
            url: format!("https://{name}.example.test"),
            ..Provider::default()
        }
    }

    fn session(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            ..SessionSummary::default()
        }
    }

    fn resource_option(kind: &str, label: &str, template: &str) -> SessionTemplateOption {
        SessionTemplateOption {
            kind: match kind {
                "file" => SessionTemplateKind::File as i32,
                "tool" => SessionTemplateKind::Tool as i32,
                _ => 0,
            },
            label: label.to_owned(),
            detail: label.to_owned(),
            template: template.to_owned(),
            metadata_json: "{}".to_owned(),
        }
    }

    fn timeline_event(cursor: u64) -> TimelineEvent {
        TimelineEvent {
            cursor,
            session_id: "session".to_owned(),
            item_id: format!("i{cursor}"),
            ..TimelineEvent::default()
        }
    }

    fn timeline_page(events: Vec<TimelineEvent>) -> crate::SessionTimelinePage {
        crate::SessionTimelinePage {
            projection_mode: crate::SessionTimelineProjectionMode::Raw,
            entries: events
                .into_iter()
                .map(|event| crate::ProjectedTimelineEvent {
                    presentation: crate::TimelinePresentation::Durable as i32,
                    mask_id: None,
                    event: Some(crate::projected_timeline_event::Event::Durable(event)),
                })
                .collect(),
            projection: None,
        }
    }

    fn complete_timeline_page(
        client: &mut Client,
        session_id: &str,
        page: crate::SessionTimelinePage,
    ) {
        let update =
            client.request_session_timeline_with_projection(session_id, None, page.projection_mode);
        let [
            ClientEffect::Session(SessionEffect::GetTimeline {
                request_id,
                baseline_mode,
                baseline_revision,
                ..
            }),
        ] = update.effects()
        else {
            panic!("timeline page effect")
        };
        client.apply(ClientEvent::Session(SessionEvent::TimelineLoaded {
            session_id: session_id.to_owned(),
            request_id: *request_id,
            baseline_mode: *baseline_mode,
            baseline_revision: *baseline_revision,
            page,
        }));
    }

    fn begin_effective_page(
        client: &mut Client,
    ) -> (u64, crate::SessionTimelineProjectionMode, Option<u64>) {
        let update = client.request_session_timeline_with_projection(
            "session",
            None,
            crate::SessionTimelineProjectionMode::Effective,
        );
        let [
            ClientEffect::Session(SessionEffect::GetTimeline {
                request_id,
                baseline_mode,
                baseline_revision,
                ..
            }),
        ] = update.effects()
        else {
            panic!("effective timeline page effect")
        };
        (*request_id, *baseline_mode, *baseline_revision)
    }

    fn apply_effective_page(
        client: &mut Client,
        (request_id, baseline_mode, baseline_revision): (
            u64,
            crate::SessionTimelineProjectionMode,
            Option<u64>,
        ),
        revision: u64,
        raw_page_boundary: Option<u64>,
        event_cursor: u64,
    ) {
        client.apply(ClientEvent::Session(SessionEvent::TimelineLoaded {
            session_id: "session".to_owned(),
            request_id,
            baseline_mode,
            baseline_revision,
            page: crate::SessionTimelinePage {
                projection_mode: crate::SessionTimelineProjectionMode::Effective,
                entries: vec![durable_projection(timeline_event(event_cursor))],
                projection: Some(crate::TimelineProjectionMetadata {
                    revision,
                    raw_page_boundary,
                    has_more: raw_page_boundary.is_some(),
                }),
            },
        }));
    }

    fn apply_timeline_failure(
        client: &mut Client,
        (request_id, baseline_mode, baseline_revision): (
            u64,
            crate::SessionTimelineProjectionMode,
            Option<u64>,
        ),
        message: &str,
    ) {
        client.apply(ClientEvent::Session(SessionEvent::TimelineLoadFailed {
            session_id: "session".to_owned(),
            request_id,
            baseline_mode,
            baseline_revision,
            error: ClientError::new(message),
        }));
    }

    fn apply_projection_reset(client: &mut Client, revision: u64, mask_id: &str, cursor: u64) {
        let key = crate::TimelineWatchKey::new_with_projection(
            "session".to_owned(),
            None,
            Vec::new(),
            None,
            crate::SessionTimelineProjectionMode::Effective,
        );
        client.apply(ClientEvent::Session(SessionEvent::TimelineWatchChanged {
            key,
            update: SessionTimelineWatchUpdate {
                sequence: revision,
                event: SessionTimelineWatchEvent::ProjectionReset {
                    entries: vec![synthetic_timeline_event(mask_id, cursor)],
                    projection: Some(crate::TimelineProjectionMetadata {
                        revision,
                        raw_page_boundary: None,
                        has_more: false,
                    }),
                },
            },
        }));
    }

    fn durable_projection(event: TimelineEvent) -> crate::ProjectedTimelineEvent {
        crate::ProjectedTimelineEvent {
            presentation: crate::TimelinePresentation::Durable as i32,
            mask_id: None,
            event: Some(crate::projected_timeline_event::Event::Durable(event)),
        }
    }

    fn synthetic_timeline_event(mask_id: &str, cursor: u64) -> crate::ProjectedTimelineEvent {
        crate::ProjectedTimelineEvent {
            presentation: crate::TimelinePresentation::MaskReplacement as i32,
            mask_id: Some(mask_id.to_owned()),
            event: Some(crate::projected_timeline_event::Event::Synthetic(
                waltr::grpc::waltr::session::v1::SyntheticTimelineEvent {
                    cursor,
                    session_id: "session".to_owned(),
                    turn: 1,
                    ..Default::default()
                },
            )),
        }
    }

    fn mcp_servers<const N: usize>(
        servers: [crate::McpServer; N],
    ) -> std::collections::HashMap<String, crate::McpServer> {
        servers
            .into_iter()
            .map(|server| (server.name.clone(), server))
            .collect()
    }

    fn mcp_server(name: &str) -> crate::McpServer {
        crate::McpServer {
            name: name.to_owned(),
            r#type: crate::McpServerType::Stdio as i32,
            command: "run".to_owned(),
            url: String::new(),
        }
    }

    fn edit_baseline(path: &str) -> ResourceEditBaselineView {
        ResourceEditBaselineView {
            path: path.to_owned(),
            persisted_etag: "absent-etag".to_owned(),
            absent_etag: "absent-etag".to_owned(),
            active_source: if path.starts_with("providers/") {
                crate::resource::provider_source(&provider("anthropic"), None, None).unwrap()
            } else {
                crate::resource::mcp_server_source(&mcp_server("filesystem"), None, None).unwrap()
            },
        }
    }

    fn only_resource_writes(update: &ClientUpdate) -> &[ResourceWrite] {
        assert_eq!(update.effects().len(), 1);
        match &update.effects()[0] {
            ClientEffect::Config(ConfigEffect::Patch { writes, .. }) => writes,
            effect => panic!("expected patch effect, got {effect:?}"),
        }
    }

    fn only_reset_config(update: &ClientUpdate) -> (&OperationKey, &[ResourceWrite]) {
        assert_eq!(update.effects().len(), 1);
        match &update.effects()[0] {
            ClientEffect::Config(ConfigEffect::Reset {
                operation_key,
                writes,
            }) => (operation_key, writes),
            effect => panic!("expected reset effect, got {effect:?}"),
        }
    }

    fn provider_write_paths(writes: &[ResourceWrite]) -> Vec<&str> {
        resource_write_paths(writes)
    }

    fn resource_write_paths(writes: &[ResourceWrite]) -> Vec<&str> {
        writes
            .iter()
            .map(|write| match write {
                ResourceWrite::Replace { path, .. } | ResourceWrite::Reset { path, .. } => {
                    path.as_str()
                }
            })
            .collect()
    }

    fn catalog_model(provider_id: &str, model_id: &str) -> CatalogModelSummary {
        CatalogModelSummary {
            model: Some(CatalogModelRef {
                provider_id: provider_id.to_owned(),
                model_id: model_id.to_owned(),
            }),
            provider_name: provider_id.to_owned(),
            model_name: model_id.to_owned(),
            ..CatalogModelSummary::default()
        }
    }

    fn catalog_match(provider_id: &str, model_id: &str) -> CatalogMatch {
        CatalogMatch {
            model: Some(CatalogModelRef {
                provider_id: provider_id.to_owned(),
                model_id: model_id.to_owned(),
            }),
            confidence: CatalogMatchConfidence::High as i32,
            ..CatalogMatch::default()
        }
    }
}
