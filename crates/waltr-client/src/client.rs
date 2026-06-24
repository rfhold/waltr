//! Shell-neutral client reducer.

use crate::coordination::{config, session, status, stream};
use crate::{
    ClientEffect, ClientError, ClientEvent, ClientState, ConfigEffect, ConfigEvent, ConfigPatch,
    Configuration, McpServer, OperationKey, Provider, ProviderEditBaseline, RuntimeDefaults,
    SessionEvent, SessionRuntimeConfigRequest, StatusEvent, TimelineQuery,
};

/// Single headless client reducer used by shells.
#[derive(Clone, Debug, Default)]
pub struct Client {
    state: ClientState,
}

impl Client {
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

    /// Apply a shell-neutral event and return resulting side-effect intents.
    pub fn apply(&mut self, event: ClientEvent) -> ClientUpdate {
        match event {
            ClientEvent::Config(event) => config::reduce(&mut self.state, event),
            ClientEvent::Status(event) => status::reduce(&mut self.state, event),
            ClientEvent::Session(event) => session::reduce(&mut self.state, event),
            ClientEvent::Stream(event) => {
                stream::reduce(&mut self.state, event);
                ClientUpdate::default()
            }
        }
    }

    /// Request that the current configuration be loaded.
    pub fn request_config(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Config(ConfigEvent::LoadRequested))
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
    pub fn create_session(&mut self) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::CreateRequested(
            operation_key,
        )))
    }

    /// Request that a server-side session be deleted.
    pub fn delete_session(&mut self, id: impl Into<String>) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(SessionEvent::DeleteRequested {
            operation_key,
            id: id.into(),
        }))
    }

    /// Request that a server-side session runtime config metadata snapshot be replaced.
    pub fn update_session_runtime_config(
        &mut self,
        id: impl Into<String>,
        runtime_config: SessionRuntimeConfigRequest,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.apply(ClientEvent::Session(
            SessionEvent::RuntimeConfigUpdateRequested {
                operation_key,
                id: id.into(),
                runtime_config: Box::new(runtime_config),
            },
        ))
    }

    /// Request that the current server-side sessions be loaded.
    pub fn request_sessions(&mut self) -> ClientUpdate {
        self.apply(ClientEvent::Session(SessionEvent::ListRequested))
    }

    /// Request that a session timeline page be loaded.
    pub fn request_session_timeline(
        &mut self,
        session_id: impl Into<String>,
        query: Option<TimelineQuery>,
    ) -> ClientUpdate {
        self.apply(ClientEvent::Session(SessionEvent::TimelineRequested {
            session_id: session_id.into(),
            query,
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
        self.apply(ClientEvent::Session(
            SessionEvent::TimelineWatchInterestRegistered {
                session_id: session_id.into(),
                after_cursor,
                channels,
                initial_limit,
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

    /// Request that system status changes be watched.
    pub fn request_status_watch(&mut self) -> ClientUpdate {
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

        for session_id in self.state.timeline_watch_session_ids() {
            let update = session::maintain_timeline_watch(&mut self.state, &session_id);
            if !update.is_empty() {
                return update;
            }
        }

        ClientUpdate::default()
    }

    /// Request that the configuration be patched.
    pub fn request_config_patch(
        &mut self,
        patch: ConfigPatch,
        expected_etag: impl Into<String>,
    ) -> ClientUpdate {
        let operation_key = self.state.next_operation_key();
        self.request_config_patch_with_key(patch, expected_etag, operation_key)
    }

    fn request_config_patch_with_key(
        &mut self,
        patch: ConfigPatch,
        expected_etag: impl Into<String>,
        operation_key: OperationKey,
    ) -> ClientUpdate {
        let mut update = self.apply(ClientEvent::Config(ConfigEvent::PatchRequested(
            operation_key.clone(),
        )));
        update
            .effects
            .push(ClientEffect::Config(ConfigEffect::Patch {
                operation_key,
                patch,
                expected_etag: expected_etag.into(),
            }));
        update
    }

    /// Request a patch that adds a provider to the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or provider names are invalid.
    pub fn add_provider(&mut self, provider: Provider) -> Result<ClientUpdate, ClientError> {
        let mut config = self.current_config()?;
        let expected_etag = config.etag.clone();
        Self::validate_provider_name(&provider.name)?;
        let name = provider.name.clone();
        if config.providers.insert(name.clone(), provider).is_some() {
            return Err(ClientError::new(format!(
                "provider name '{name}' is already used"
            )));
        }
        Self::validate_providers(&config.providers)?;
        Ok(self.request_config_patch(providers_patch(config.providers), expected_etag))
    }

    /// Request a patch that replaces a provider in the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded, the provider is missing, or names are invalid.
    pub fn update_provider(
        &mut self,
        original_name: &str,
        provider: Provider,
    ) -> Result<ClientUpdate, ClientError> {
        let mut config = self.current_config()?;
        let expected_etag = config.etag.clone();
        Self::validate_provider_name(original_name)?;
        Self::validate_provider_name(&provider.name)?;
        let provider_name = provider.name.clone();
        if provider_name != original_name && config.providers.contains_key(&provider_name) {
            return Err(ClientError::new(format!(
                "provider name '{provider_name}' is already used"
            )));
        }

        config
            .providers
            .remove(original_name)
            .ok_or_else(|| ClientError::new(format!("provider '{original_name}' not found")))?;

        config.providers.insert(provider_name, provider);
        Self::validate_providers(&config.providers)?;
        Ok(self.request_config_patch(providers_patch(config.providers), expected_etag))
    }

    /// Request a patch that deletes a provider from the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded, the provider is missing, or names are invalid.
    pub fn delete_provider(&mut self, name: &str) -> Result<ClientUpdate, ClientError> {
        let mut config = self.current_config()?;
        let expected_etag = config.etag.clone();
        if config.providers.remove(name).is_none() {
            return Err(ClientError::new(format!("provider '{name}' not found")));
        }

        Self::validate_providers(&config.providers)?;
        Ok(self.request_config_patch(providers_patch(config.providers), expected_etag))
    }

    /// Request a patch that adds an MCP server to the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or the server key is invalid.
    pub fn add_mcp_server(&mut self, server: McpServer) -> Result<ClientUpdate, ClientError> {
        let mut config = self.current_config()?;
        let expected_etag = config.etag.clone();
        Self::validate_mcp_server_key(&server.name)?;
        let key = server.name.clone();
        if config.mcp_servers.insert(key.clone(), server).is_some() {
            return Err(ClientError::new(format!(
                "MCP server key '{key}' is already used"
            )));
        }
        Self::validate_mcp_servers(&config.mcp_servers)?;
        Ok(self.request_config_patch(mcp_servers_patch(config.mcp_servers), expected_etag))
    }

    /// Request a patch that replaces an MCP server in the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded, the server is missing, or keys are invalid.
    pub fn update_mcp_server(
        &mut self,
        original_name: &str,
        server: McpServer,
    ) -> Result<ClientUpdate, ClientError> {
        let mut config = self.current_config()?;
        let expected_etag = config.etag.clone();
        Self::validate_mcp_server_key(original_name)?;
        Self::validate_mcp_server_key(&server.name)?;
        let server_key = server.name.clone();
        if server_key != original_name && config.mcp_servers.contains_key(&server_key) {
            return Err(ClientError::new(format!(
                "MCP server key '{server_key}' is already used"
            )));
        }

        config
            .mcp_servers
            .remove(original_name)
            .ok_or_else(|| ClientError::new(format!("MCP server '{original_name}' not found")))?;

        config.mcp_servers.insert(server_key, server);
        Self::validate_mcp_servers(&config.mcp_servers)?;
        Ok(self.request_config_patch(mcp_servers_patch(config.mcp_servers), expected_etag))
    }

    /// Request a patch that deletes an MCP server from the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or the server is missing.
    pub fn delete_mcp_server(&mut self, name: &str) -> Result<ClientUpdate, ClientError> {
        let mut config = self.current_config()?;
        let expected_etag = config.etag.clone();
        if config.mcp_servers.remove(name).is_none() {
            return Err(ClientError::new(format!("MCP server '{name}' not found")));
        }

        Self::validate_mcp_servers(&config.mcp_servers)?;
        Ok(self.request_config_patch(mcp_servers_patch(config.mcp_servers), expected_etag))
    }

    /// Request a patch that replaces runtime defaults in the latest configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration has loaded or defaults reference missing config.
    pub fn set_runtime_defaults(
        &mut self,
        defaults: Option<RuntimeDefaults>,
    ) -> Result<ClientUpdate, ClientError> {
        let config = self.current_config()?;
        let expected_etag = config.etag.clone();
        if let Some(defaults) = defaults.as_ref() {
            Self::validate_runtime_defaults(defaults, &config)?;
        }

        Ok(self.request_config_patch(runtime_defaults_patch(defaults), expected_etag))
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
        let expected_etag = config.etag.clone();
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

        new_name.clone_into(&mut provider.name);
        config.providers.insert(new_name.to_owned(), provider);
        Self::validate_providers(&config.providers)?;
        Ok(self.request_config_patch(providers_patch(config.providers), expected_etag))
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

        Ok(ProviderEditBaseline {
            name: name.to_owned(),
            etag: config.etag.clone(),
            provider: provider.clone(),
        })
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

    fn validate_providers(
        providers: &std::collections::HashMap<String, Provider>,
    ) -> Result<(), ClientError> {
        for (name, provider) in providers {
            Self::validate_provider_name(name)?;
            if provider.name != *name {
                return Err(ClientError::new(format!(
                    "provider key '{name}' must match provider name"
                )));
            }
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

        Ok(())
    }

    fn validate_mcp_servers(
        servers: &std::collections::HashMap<String, McpServer>,
    ) -> Result<(), ClientError> {
        for (key, server) in servers {
            Self::validate_mcp_server_key(key)?;
            if server.name != *key {
                return Err(ClientError::new(format!(
                    "MCP server key '{key}' must match server name"
                )));
            }
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

        Ok(())
    }

    fn validate_runtime_defaults(
        defaults: &RuntimeDefaults,
        config: &Configuration,
    ) -> Result<(), ClientError> {
        if defaults.provider_key.trim().is_empty() && defaults.model_key.trim().is_empty() {
            for key in &defaults.enabled_mcp_server_keys {
                if !config.mcp_servers.contains_key(key) {
                    return Err(ClientError::new(format!(
                        "runtime default MCP server '{key}' not found"
                    )));
                }
            }
            return Ok(());
        }

        let provider = config
            .providers
            .get(&defaults.provider_key)
            .ok_or_else(|| {
                ClientError::new(format!(
                    "runtime default provider '{}' not found",
                    defaults.provider_key
                ))
            })?;
        if defaults.model_key.trim().is_empty() {
            return Err(ClientError::new(
                "runtime default model id cannot be empty when provider is set",
            ));
        }
        if !provider.models.contains_key(&defaults.model_key) {
            return Err(ClientError::new(format!(
                "runtime default model '{}' not found in provider '{}'",
                defaults.model_key, defaults.provider_key
            )));
        }
        for key in &defaults.enabled_mcp_server_keys {
            if !config.mcp_servers.contains_key(key) {
                return Err(ClientError::new(format!(
                    "runtime default MCP server '{key}' not found"
                )));
            }
        }

        Ok(())
    }
}

const fn providers_patch(
    providers: std::collections::HashMap<String, Provider>,
) -> crate::ConfigPatch {
    crate::ConfigPatch {
        providers: Some(providers),
        mcp_servers: None,
        runtime_defaults: None,
    }
}

const fn mcp_servers_patch(
    mcp_servers: std::collections::HashMap<String, McpServer>,
) -> crate::ConfigPatch {
    crate::ConfigPatch {
        providers: None,
        mcp_servers: Some(mcp_servers),
        runtime_defaults: None,
    }
}

const fn runtime_defaults_patch(defaults: Option<RuntimeDefaults>) -> crate::ConfigPatch {
    crate::ConfigPatch {
        providers: None,
        mcp_servers: None,
        runtime_defaults: defaults,
    }
}

/// Result of applying an event to the shared client.
#[derive(Clone, Debug, Default, PartialEq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::transport::SessionTimelineWatchUpdate;
    use crate::{
        ConfigWatchUpdate, OperationStatus, SessionEffect, SessionSummary, SessionWatchEvent,
        SessionWatchUpdate, StatusEffect, StreamEvent, StreamStatus, SystemStatus, TimelineEvent,
        watch_reconnect_delay_ms,
    };

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

        let update = client.apply(ClientEvent::Config(ConfigEvent::Loaded(config)));

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

        client.apply(ClientEvent::Config(ConfigEvent::Loaded(config.clone())));
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
    fn watch_events_update_config_or_watch_error() {
        let mut client = Client::new();
        let config = config_with_provider("anthropic", "etag-1");

        client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));
        assert_eq!(client.state().config.watch.status, OperationStatus::Loading);
        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::Watch)]
        );

        client.apply(ClientEvent::Config(ConfigEvent::WatchStarted));
        assert_eq!(client.state().config.watch.status, OperationStatus::Opened);

        client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                config: Some(config.clone()),
                ..ConfigWatchUpdate::default()
            },
        )));
        assert_eq!(
            client.state().config.watch.status,
            OperationStatus::Received
        );
        assert_eq!(client.state().config.current.as_ref(), Some(&config));

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                error: "stream broke".to_owned(),
                ..ConfigWatchUpdate::default()
            },
        )));
        assert_eq!(client.state().config.watch.status, OperationStatus::Failed);
        assert_eq!(
            client.state().config.watch.error.as_deref(),
            Some("stream broke")
        );
        assert_eq!(client.state().config.current.as_ref(), Some(&config));
        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::SleepReconnect {
                delay_ms: 500
            })]
        );
    }

    #[test]
    fn watch_events_merge_partial_updates_when_etag_matches() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            providers: providers([provider("anthropic")]),
            mcp_servers: mcp_servers([mcp_server("filesystem")]),
            runtime_defaults: None,
        });

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                previous_etag: "etag-1".to_owned(),
                etag: "etag-2".to_owned(),
                changed_keys: vec!["providers".to_owned()],
                config: Some(config_with_provider("openai", "etag-2")),
                ..ConfigWatchUpdate::default()
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
        assert_eq!(current.mcp_servers, mcp_servers([mcp_server("filesystem")]));

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                previous_etag: "etag-2".to_owned(),
                etag: "etag-3".to_owned(),
                changed_keys: vec!["mcp_servers".to_owned()],
                config: Some(Configuration {
                    etag: "etag-3".to_owned(),
                    mcp_servers: mcp_servers([mcp_server("github")]),
                    ..Configuration::default()
                }),
                ..ConfigWatchUpdate::default()
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
        assert_eq!(current.providers, providers([provider("openai")]));
        assert_eq!(current.mcp_servers, mcp_servers([mcp_server("github")]));
    }

    #[test]
    fn watch_events_refetch_when_previous_etag_is_out_of_sync() {
        let mut client = client_with_config(config_with_provider("anthropic", "etag-1"));

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchChanged(
            ConfigWatchUpdate {
                previous_etag: "etag-stale".to_owned(),
                etag: "etag-2".to_owned(),
                changed_keys: vec!["providers".to_owned()],
                config: Some(config_with_provider("openai", "etag-2")),
                ..ConfigWatchUpdate::default()
            },
        )));

        assert_eq!(
            client.state().config.watch.status,
            OperationStatus::Received
        );
        assert_eq!(
            client.state().config.current.as_ref().unwrap().etag,
            "etag-1"
        );
        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::Load)]
        );
    }

    #[test]
    fn config_watch_waits_when_status_is_not_ready() {
        let mut client = Client::new();

        let update = client.apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));

        assert!(update.is_empty());
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);

        let update = client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));

        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::Watch)]
        );
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
        assert_eq!(
            update.effects(),
            &[ClientEffect::Config(ConfigEffect::Watch)]
        );

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
                delay_ms: 500
            })]
        );

        let config_update = client.apply(ClientEvent::Config(ConfigEvent::ReconnectReady));
        assert!(config_update.is_empty());
        assert_eq!(client.state().config.watch.status, OperationStatus::Waiting);

        let recovery_update = client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
        assert_eq!(
            recovery_update.effects(),
            &[ClientEffect::Config(ConfigEffect::Watch)]
        );
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

        client.apply(ClientEvent::Session(SessionEvent::TimelineWatchChanged(
            SessionTimelineWatchUpdate {
                sequence: 1,
                event: timeline_event(10),
            },
        )));
        client.apply(ClientEvent::Session(SessionEvent::TimelineLoaded {
            session_id: "session".to_owned(),
            events: vec![timeline_event(8), timeline_event(9)],
        }));

        let timeline = client
            .state()
            .sessions
            .timelines
            .get("session")
            .expect("timeline exists");
        assert_eq!(
            timeline.events.keys().copied().collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        assert_eq!(timeline.load.status, OperationStatus::Ready);
        assert_eq!(timeline.watch.status, OperationStatus::Received);
    }

    #[test]
    fn session_create_completion_upserts_current_session() {
        let mut client = Client::new();
        let operation_key = OperationKey::from("create-session");

        client.apply(ClientEvent::Session(SessionEvent::Created {
            operation_key,
            session: session("one"),
        }));

        assert!(client.state().sessions.current.contains_key("one"));
        assert_eq!(
            client.state().sessions.create.status,
            OperationStatus::Ready
        );
    }

    #[test]
    fn session_delete_completion_removes_current_session() {
        let mut client = Client::new();
        client.apply(ClientEvent::Session(SessionEvent::Listed(vec![session(
            "one",
        )])));
        let operation_key = OperationKey::from("delete-session");

        client.apply(ClientEvent::Session(SessionEvent::Deleted {
            operation_key,
            id: "one".to_owned(),
        }));

        assert!(!client.state().sessions.current.contains_key("one"));
        assert_eq!(
            client.state().sessions.delete.status,
            OperationStatus::Ready
        );
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
        assert_eq!(
            recovery_update.effects(),
            &[ClientEffect::Session(SessionEffect::Watch)]
        );
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
        assert_eq!(
            update.effects(),
            &[ClientEffect::Status(StatusEffect::Watch)]
        );

        let update = client.apply(ClientEvent::Status(StatusEvent::WatchChanged(
            SystemStatus { ready: true },
        )));
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

        let update = client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
            ClientError::new("lost status"),
        )));
        assert_eq!(client.state().status.watch.status, OperationStatus::Failed);
        assert!(!client.state().is_system_ready());
        assert_eq!(
            client.state().status.watch.error.as_deref(),
            Some("lost status")
        );
        assert_eq!(
            update.effects(),
            &[ClientEffect::Status(StatusEffect::SleepReconnect {
                delay_ms: 500
            })]
        );

        for expected_delay in [1_000, 2_000, 5_000, 5_000] {
            let update = client.apply(ClientEvent::Status(StatusEvent::WatchFailed(
                ClientError::new("lost status"),
            )));
            assert_eq!(
                update.effects(),
                &[ClientEffect::Status(StatusEffect::SleepReconnect {
                    delay_ms: expected_delay
                })]
            );
        }

        let update = client.apply(ClientEvent::Status(StatusEvent::ReconnectReady));
        assert_eq!(
            update.effects(),
            &[ClientEffect::Status(StatusEffect::Watch)]
        );
        assert_eq!(watch_reconnect_delay_ms(99), 5_000);
    }

    #[test]
    fn provider_commands_emit_provider_map_patch_effects() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            providers: providers([provider("anthropic")]),
            ..Configuration::default()
        });

        let update = client
            .add_provider(provider("openai"))
            .expect("add provider");
        let (patch, expected_etag) = only_patch_config(&update);
        assert_eq!(expected_etag, "etag-1");
        assert!(patch.mcp_servers.is_none());
        assert_eq!(provider_patch_names(patch), vec!["anthropic", "openai"]);

        let update = client
            .update_provider("anthropic", provider("claude"))
            .expect("update provider");
        let (patch, _) = only_patch_config(&update);
        assert!(patch.mcp_servers.is_none());
        assert_eq!(provider_patch_names(patch), vec!["claude"]);

        let update = client
            .rename_provider("anthropic", "renamed")
            .expect("rename provider");
        let (patch, _) = only_patch_config(&update);
        assert!(patch.mcp_servers.is_none());
        assert_eq!(provider_patch_names(patch), vec!["renamed"]);

        let update = client
            .delete_provider("anthropic")
            .expect("delete provider");
        let (patch, _) = only_patch_config(&update);
        assert!(patch.mcp_servers.is_none());
        assert!(
            patch
                .providers
                .as_ref()
                .is_some_and(std::collections::HashMap::is_empty)
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
    fn mcp_server_commands_emit_mcp_map_patch_effects() {
        let mut client = client_with_config(Configuration {
            etag: "etag-1".to_owned(),
            mcp_servers: mcp_servers([mcp_server("filesystem")]),
            ..Configuration::default()
        });

        let update = client
            .add_mcp_server(mcp_server("github"))
            .expect("add mcp server");
        let (patch, expected_etag) = only_patch_config(&update);
        assert_eq!(expected_etag, "etag-1");
        assert!(patch.providers.is_none());
        assert_eq!(mcp_server_patch_names(patch), vec!["filesystem", "github"]);

        let update = client
            .update_mcp_server("filesystem", mcp_server("fs"))
            .expect("update mcp server");
        let (patch, _) = only_patch_config(&update);
        assert!(patch.providers.is_none());
        assert_eq!(mcp_server_patch_names(patch), vec!["fs"]);

        let update = client
            .delete_mcp_server("filesystem")
            .expect("delete mcp server");
        let (patch, _) = only_patch_config(&update);
        assert!(patch.providers.is_none());
        assert!(
            patch
                .mcp_servers
                .as_ref()
                .is_some_and(std::collections::HashMap::is_empty)
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
            .update_mcp_server("filesystem", mcp_server(""))
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
                config: Some(Configuration {
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
                config: Some(Configuration {
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
        client.apply(ClientEvent::Config(ConfigEvent::Loaded(config)));
        client
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
            ..Provider::default()
        }
    }

    fn session(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            ..SessionSummary::default()
        }
    }

    fn timeline_event(cursor: u64) -> TimelineEvent {
        TimelineEvent {
            cursor,
            session_id: "session".to_owned(),
            ..TimelineEvent::default()
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

    fn only_patch_config(update: &ClientUpdate) -> (&ConfigPatch, &str) {
        assert_eq!(update.effects().len(), 1);
        match &update.effects()[0] {
            ClientEffect::Config(ConfigEffect::Patch {
                patch,
                expected_etag,
                ..
            }) => (patch, expected_etag),
            effect => panic!("expected patch effect, got {effect:?}"),
        }
    }

    fn provider_patch_names(patch: &ConfigPatch) -> Vec<&str> {
        let mut names: Vec<&str> = patch
            .providers
            .as_ref()
            .expect("provider patch should include providers")
            .values()
            .map(|provider| provider.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    fn mcp_server_patch_names(patch: &ConfigPatch) -> Vec<&str> {
        let mut names: Vec<&str> = patch
            .mcp_servers
            .as_ref()
            .expect("mcp patch should include mcp servers")
            .values()
            .map(|server| server.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }
}
