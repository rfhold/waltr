use super::{
    AgentSessionRuntimeError, CredentialStore, InferenceProviderSettings, InferenceSettings,
    OAuthCredential, SessionRecord,
};
use crate::{
    AgentRuntime, AgentRuntimeBuilder, AuthTarget, AuthenticationFailure, BoxCompletionStream,
    BoxStreamFuture, CompletionRequest, LlmError, LlmStream, ModelApi, ModelCost, ModelRef,
    ProviderId,
    provider::{
        AnthropicMessages, OpenAiChatCompletions, OpenAiReasoningConfig, OpenAiReasoningEffort,
        OpenAiReasoningSummary, OpenAiResponses, OpenAiResponsesWebSocket,
    },
};
use futures_util::StreamExt as _;
use std::{collections::BTreeMap, sync::Arc};
mod headers;
mod oauth;
use headers::{configured_credential, is_credential_header, resolve_provider_headers};
pub use oauth::{CredentialRefreshCoordinator, CredentialRefreshError, RefreshMode};
const ENV_PREFIX: &str = "${env:";
const OAUTH_REFRESH_AHEAD_MS: i64 = 5 * 60 * 1000;
type RuntimeProviderOverride = Arc<
    dyn Fn(&InferenceSettings) -> Result<AgentRuntimeProvider, AgentSessionRuntimeError>
        + Send
        + Sync,
>;
/// Provider-only input for session runtime construction.
#[derive(Clone)]
pub struct AgentRuntimeProvider {
    model: ModelRef,
    provider: Arc<dyn LlmStream>,
    model_cost: Option<ModelCost>,
}

impl AgentRuntimeProvider {
    /// Create provider input for a model.
    #[must_use]
    pub fn new<P>(model: ModelRef, provider: Arc<P>) -> Self
    where
        P: LlmStream + 'static,
    {
        Self {
            model,
            provider,
            model_cost: None,
        }
    }

    /// Set model pricing used to journal per-turn cost.
    #[must_use]
    pub fn with_model_cost(mut self, cost: ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
    }

    /// Start ordinary runtime assembly with the resolved provider and pricing.
    pub fn into_builder(self) -> AgentRuntimeBuilder {
        builder_with_model_cost(
            AgentRuntimeBuilder::new_dyn(self.model, self.provider),
            self.model_cost,
        )
    }
}

impl std::fmt::Debug for AgentRuntimeProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentRuntimeProvider")
            .field("model", &self.model)
            .field("model_cost", &self.model_cost)
            .finish_non_exhaustive()
    }
}
/// Provider construction and generic host binding; no Waltr capability configuration.
#[derive(Clone)]
pub struct AgentSessionRuntimeFactory {
    pub credential_store: CredentialStore,
    runtime_provider_override: Option<RuntimeProviderOverride>,
    host: Option<Arc<dyn crate::AgentHostRuntimeFactory>>,
    pub(crate) run_events: Option<Arc<dyn crate::SessionEventSink>>,
    pub(crate) run_completion: Option<Arc<dyn super::SessionCompletionCoordinationHandler>>,
}
impl std::fmt::Debug for AgentSessionRuntimeFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSessionRuntimeFactory")
            .field("host", &self.host.is_some())
            .finish_non_exhaustive()
    }
}
impl AgentSessionRuntimeFactory {
    pub fn new(credential_store: impl Into<CredentialStore>) -> Self {
        Self {
            credential_store: credential_store.into(),
            runtime_provider_override: None,
            host: None,
            run_events: None,
            run_completion: None,
        }
    }
    pub fn with_host_runtime_factory(
        mut self,
        host: Arc<dyn crate::AgentHostRuntimeFactory>,
    ) -> Self {
        self.host = Some(host);
        self
    }
    pub(crate) fn same_host_binding(&self, other: &Self) -> bool {
        let host = match (&self.host, &other.host) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let events = match (&self.run_events, &other.run_events) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let completion = match (&self.run_completion, &other.run_completion) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        host && events && completion
    }
    pub(crate) async fn build_for_discovery(
        &self,
        record: &SessionRecord,
    ) -> crate::Result<AgentRuntime> {
        self.host
            .as_ref()
            .ok_or_else(|| LlmError::Protocol {
                provider: "host-runtime",
                message: "host runtime factory is not configured".into(),
            })?
            .build_for_discovery(record)
            .await
    }

    pub(crate) async fn build(&self, record: &SessionRecord) -> crate::Result<AgentRuntime> {
        if super::super::tool::host_dispatch_unsettled(record) {
            return Err(LlmError::Protocol {
                provider: "host-runtime",
                message: "interrupted host dispatch requires reconciliation, not runtime replay"
                    .into(),
            });
        }
        self.host
            .as_ref()
            .ok_or_else(|| LlmError::Protocol {
                provider: "host-runtime",
                message: "host runtime factory is not configured".into(),
            })?
            .build(record)
            .await
    }
    pub(crate) fn configuration_for_cycle(
        &self,
        record: &SessionRecord,
    ) -> crate::Result<Option<crate::HostConfigurationUpdate>> {
        self.host
            .as_ref()
            .map_or(Ok(None), |host| host.configuration_for_cycle(record))
    }
    pub(crate) async fn retire(&self, session: &crate::SessionId) -> crate::Result<()> {
        if let Some(host) = &self.host {
            host.retire(session).await?;
        }
        Ok(())
    }
    pub(crate) async fn retire_all(&self) -> crate::Result<()> {
        if let Some(host) = &self.host {
            host.retire_all().await?;
        }
        Ok(())
    }
    /// Bind host callbacks to this exact run origin, including queued submissions.
    #[must_use]
    pub fn with_run_coordination(
        mut self,
        events: Arc<dyn crate::SessionEventSink>,
        completion: Arc<dyn super::SessionCompletionCoordinationHandler>,
    ) -> Self {
        self.run_events = Some(events);
        self.run_completion = Some(completion);
        self
    }
    #[must_use]
    pub fn with_runtime_provider(
        mut self,
        build: impl Fn(&InferenceSettings) -> Result<AgentRuntimeProvider, AgentSessionRuntimeError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.runtime_provider_override = Some(Arc::new(build));
        self
    }
    /// Resolve provider assembly without interpreting host configuration.
    pub async fn provider_builder(
        &self,
        settings: &InferenceSettings,
    ) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError> {
        if let Some(build) = &self.runtime_provider_override {
            return Ok(build(settings)?.into_builder());
        }
        self.authenticated_provider_builder(settings).await
    }
    pub async fn provider_for_settings(
        &self,
        settings: &InferenceSettings,
    ) -> Result<Arc<dyn LlmStream>, AgentSessionRuntimeError> {
        Ok(self.provider_builder(settings).await?.build().provider)
    }
    #[allow(clippy::too_many_lines)]
    async fn authenticated_provider_builder(
        &self,
        settings: &InferenceSettings,
    ) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError> {
        let provider = &settings.provider;
        let headers = resolve_provider_headers(&provider.headers)?;
        let oauth = self.oauth_access(provider, &headers).await?;
        let model_id = provider.model_id.clone();
        let endpoint_url = (!provider.url.trim().is_empty()).then_some(provider.url.as_str());

        match provider.api {
            ModelApi::AnthropicMessages => {
                ensure_no_reasoning_effort(settings, "Anthropic Messages")?;
                let oauth_active = oauth.is_some();
                let oauth_billing_version = if oauth_active {
                    configured_claude_code_version(&headers)?.map(ToOwned::to_owned)
                } else {
                    None
                };
                let mut provider_impl = AnthropicMessages::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = provider_impl.with_oauth_billing_version(oauth_billing_version);
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
            ModelApi::OpenAiChatCompletions => {
                ensure_no_reasoning_effort(settings, "OpenAI Chat Completions")?;
                let oauth_active = oauth.is_some();
                let mut provider_impl = OpenAiChatCompletions::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
            ModelApi::OpenAiResponses => {
                let oauth_active = oauth.is_some();
                let mut provider_impl = OpenAiResponses::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                if let Some(reasoning) = openai_reasoning_config(settings)? {
                    provider_impl = provider_impl.with_reasoning(reasoning);
                }
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
            ModelApi::OpenAiResponsesWebSocket => {
                let oauth_active = oauth.is_some();
                let mut provider_impl = OpenAiResponsesWebSocket::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                if let Some(reasoning) = openai_reasoning_config(settings)? {
                    provider_impl = provider_impl.with_reasoning(reasoning);
                }
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
        }
    }

    async fn oauth_access(
        &self,
        provider: &InferenceProviderSettings,
        resolved_headers: &BTreeMap<String, String>,
    ) -> Result<Option<OAuthCredential>, AgentSessionRuntimeError> {
        if configured_credential(resolved_headers).is_some() {
            return Ok(None);
        }
        let Some(definition) = provider.oauth.as_ref() else {
            return Ok(None);
        };
        let credential = self
            .refresh_coordinator()
            .refresh_provider(&provider.key, definition, RefreshMode::Normal)
            .await
            .map_err(|error| refresh_runtime_error(&error))?;
        credential
            .map(Some)
            .ok_or_else(|| AgentSessionRuntimeError::Authentication {
                target: AuthTarget::provider(&provider.key),
                message: "provider OAuth credential is required".to_owned(),
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                host_context: None,
            })
    }
    fn refresh_coordinator(&self) -> CredentialRefreshCoordinator {
        CredentialRefreshCoordinator::new(self.credential_store.clone())
    }
}
struct TargetedProvider<T> {
    inner: T,
    target: AuthTarget,
}

impl<T> TargetedProvider<T> {
    fn new(inner: T, provider_key: &str) -> Self {
        Self {
            inner,
            target: AuthTarget::provider(provider_key),
        }
    }
}

impl<T> LlmStream for TargetedProvider<T>
where
    T: LlmStream,
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn transient_api(&self) -> Option<crate::TransientInputApi> {
        self.inner.transient_api()
    }

    fn transient_input(&self) -> Option<&crate::TransientInput> {
        self.inner.transient_input()
    }

    fn preflight_transient_request(
        &self,
        request: &CompletionRequest<'_>,
        input: &crate::AdmittedTransientInput,
    ) -> crate::Result<()> {
        self.inner
            .preflight_transient_request(request, input)
            .map_err(|error| provider_authentication_error(error, &self.target))
    }

    fn stream_transient_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        input: &'a crate::AdmittedTransientInput,
    ) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let stream = self
                .inner
                .stream_transient_request(request, input)
                .await
                .map_err(|error| provider_authentication_error(error, &self.target))?;
            Ok(Box::pin(stream.map({
                let target = self.target.clone();
                move |result| result.map_err(|error| provider_authentication_error(error, &target))
            })) as BoxCompletionStream)
        })
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<serde_json::Value> {
        self.inner.project_request(request)
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let stream = self
                .inner
                .stream_request(request)
                .await
                .map_err(|error| provider_authentication_error(error, &self.target))?;
            Ok(Box::pin(stream.map({
                let target = self.target.clone();
                move |result| result.map_err(|error| provider_authentication_error(error, &target))
            })) as BoxCompletionStream)
        })
    }
}

fn provider_authentication_error(error: LlmError, target: &AuthTarget) -> LlmError {
    let rejected = match &error {
        LlmError::Api {
            status: Some(401), ..
        } => true,
        LlmError::WebSocket(error) => matches!(
            error,
            tokio_tungstenite::tungstenite::Error::Http(response)
                if response.status() == http::StatusCode::UNAUTHORIZED
        ),
        _ => false,
    };
    if rejected {
        LlmError::AuthenticationFailure {
            failure: AuthenticationFailure {
                target: target.clone(),
                reason: crate::AuthenticationFailureReason::CredentialRejected,
                host_context: None,
            },
            message: "provider rejected the configured credential".to_owned(),
        }
    } else {
        error
    }
}

fn refresh_runtime_error(error: &CredentialRefreshError) -> AgentSessionRuntimeError {
    if error.is_internal() {
        AgentSessionRuntimeError::Internal(error.message().to_owned())
    } else {
        AgentSessionRuntimeError::Authentication {
            target: error.target().clone(),
            message: error.message().to_owned(),
            reason: error.reason(),
            host_context: None,
        }
    }
}

const fn provider_id(api: ModelApi) -> ProviderId {
    match api {
        ModelApi::AnthropicMessages => ProviderId::Anthropic,
        ModelApi::OpenAiChatCompletions
        | ModelApi::OpenAiResponses
        | ModelApi::OpenAiResponsesWebSocket => ProviderId::OpenAi,
    }
}

fn builder_with_model_cost(
    mut builder: AgentRuntimeBuilder,
    model_cost: Option<ModelCost>,
) -> AgentRuntimeBuilder {
    if let Some(cost) = model_cost {
        builder = builder.with_model_cost(cost);
    }
    builder
}

fn ensure_no_reasoning_effort(
    settings: &InferenceSettings,
    provider_name: &str,
) -> Result<(), AgentSessionRuntimeError> {
    if settings.provider.reasoning_effort.is_some() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
            "{provider_name} does not support session reasoning effort"
        )));
    }
    Ok(())
}

fn openai_reasoning_config(
    settings: &InferenceSettings,
) -> Result<Option<OpenAiReasoningConfig>, AgentSessionRuntimeError> {
    let Some(effort) = settings.provider.reasoning_effort.as_deref() else {
        return Ok(None);
    };
    let effort = match effort {
        "none" => OpenAiReasoningEffort::None,
        "minimal" => OpenAiReasoningEffort::Minimal,
        "low" => OpenAiReasoningEffort::Low,
        "medium" => OpenAiReasoningEffort::Medium,
        "high" => OpenAiReasoningEffort::High,
        "xhigh" => OpenAiReasoningEffort::XHigh,
        value => {
            return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
                "reasoning effort '{value}' is not supported by OpenAI Responses"
            )));
        }
    };
    Ok(Some(OpenAiReasoningConfig {
        effort,
        summary: Some(OpenAiReasoningSummary::Auto),
    }))
}

fn apply_config_headers<T>(
    mut provider: T,
    headers: &BTreeMap<String, String>,
    oauth_active: bool,
) -> T
where
    T: ConfigurableHeaders,
{
    for (name, value) in headers {
        if oauth_active && is_credential_header(name) {
            continue;
        }
        provider = provider.with_header(name, value);
    }
    provider
}

fn configured_claude_code_version(
    headers: &BTreeMap<String, String>,
) -> Result<Option<&str>, AgentSessionRuntimeError> {
    let mut user_agents = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"));
    let Some((_, user_agent)) = user_agents.next() else {
        return Ok(None);
    };
    if user_agents.next().is_some() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(
            "Anthropic OAuth provider must not configure more than one user-agent header"
                .to_owned(),
        ));
    }
    let Some(version) = user_agent.strip_prefix("claude-code/") else {
        return Ok(None);
    };
    Ok((!version.is_empty()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_')))
    .then_some(version))
}

fn apply_oauth_headers<T>(mut provider: T, oauth: &OAuthCredential) -> T
where
    T: ConfigurableHeaders,
{
    provider = provider.with_header(
        "authorization",
        &format!("Bearer {}", oauth.token.access_token.secret()),
    );
    if let Some(account_id) = oauth
        .provider_metadata()
        .and_then(|metadata| metadata.get("account_id"))
    {
        provider = provider
            .with_header("chatgpt-account-id", account_id)
            .with_header(
                "user-agent",
                &format!("waltr/{}", env!("CARGO_PKG_VERSION")),
            );
    }
    provider
}

trait ConfigurableHeaders: Sized {
    fn with_header(self, name: &str, value: &str) -> Self;
}

impl ConfigurableHeaders for AnthropicMessages {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

impl ConfigurableHeaders for OpenAiChatCompletions {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

impl ConfigurableHeaders for OpenAiResponses {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

impl ConfigurableHeaders for OpenAiResponsesWebSocket {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod transient_tests;
