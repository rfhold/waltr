//! Browser WebAssembly bindings for the shared Waltr client.

use std::{cell::RefCell, rc::Rc};

use futures_channel::{mpsc, oneshot};
use futures_util::{
    StreamExt,
    future::{Either, select},
    lock::Mutex,
    pin_mut,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use waltr::grpc::waltr::{catalog::v1 as catalog_proto, session::v1 as session_proto};
use serde::{Deserialize, Serialize};

use crate::{
    ClientCommand, ClientCommandRequest, ClientRuntime, ClientStateStreamEvent, WaltrTransport,
    transport::DefaultGrpcTransport,
};

#[wasm_bindgen(
    inline_js = "export function sleepMs(ms) { return new Promise((resolve) => setTimeout(resolve, ms)); }"
)]
unsafe extern "C" {
    #[wasm_bindgen(js_name = sleepMs)]
    async fn sleep_ms(milliseconds: u32);
}

/// Browser-facing shared reducer client.
#[derive(Debug, Clone)]
#[wasm_bindgen]
pub struct WaltrClient {
    endpoint: String,
    commands: mpsc::UnboundedSender<ClientCommandRequest>,
    latest_snapshot: Rc<RefCell<String>>,
    snapshots: Rc<Mutex<mpsc::UnboundedReceiver<String>>>,
}

#[wasm_bindgen]
#[allow(tail_expr_drop_order)]
impl WaltrClient {
    /// Create a browser client for an optional API base URL.
    ///
    /// Blank or omitted values use the current browser origin, allowing Vite's
    /// development proxy to handle `/waltr...` requests without CORS.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new(endpoint: Option<String>) -> Self {
        let endpoint = normalize_endpoint(endpoint);
        let runtime =
            ClientRuntime::for_endpoint(endpoint.clone()).into_state_stream(browser_sleep);
        let latest_snapshot = Rc::new(RefCell::new(
            serde_json::to_string(&runtime.snapshot()).unwrap_or_else(|error| error.to_string()),
        ));
        let (commands, command_receiver) = mpsc::unbounded();
        let (snapshots, snapshot_receiver) = mpsc::unbounded();

        spawn_client_loop(
            runtime,
            command_receiver,
            snapshots,
            Rc::clone(&latest_snapshot),
        );

        Self {
            endpoint,
            commands,
            latest_snapshot,
            snapshots: Rc::new(Mutex::new(snapshot_receiver)),
        }
    }

    /// Create a browser client that targets the current origin.
    #[wasm_bindgen(js_name = forCurrentOrigin)]
    #[must_use]
    pub fn for_current_origin() -> Self {
        Self::new(None)
    }

    /// Return the normalized endpoint base URL.
    #[wasm_bindgen(js_name = endpoint)]
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    /// Return a JSON snapshot of the shared reducer state.
    #[wasm_bindgen(js_name = snapshotJson)]
    pub async fn snapshot_json(&self) -> Result<String, JsValue> {
        Ok(self.latest_snapshot.borrow().clone())
    }

    /// Dispatch a JSON command through the shared reducer runtime.
    #[wasm_bindgen(js_name = dispatchJson)]
    pub async fn dispatch_json(&self, command_json: String) -> Result<String, JsValue> {
        let command: ClientCommand = serde_json::from_str(&command_json).map_err(js_error)?;
        let (result, receiver) = oneshot::channel();

        self.commands
            .unbounded_send(ClientCommandRequest::with_result(command, result))
            .map_err(js_error)?;

        let snapshot = receiver.await.map_err(js_error)?.map_err(js_error)?;

        serde_json::to_string(&snapshot).map_err(js_error)
    }

    /// Search the models.dev catalog and return a JSON response.
    #[wasm_bindgen(js_name = searchCatalogModelsJson)]
    pub async fn search_catalog_models_json(
        &self,
        request_json: String,
    ) -> Result<String, JsValue> {
        let request: CatalogSearchRequest =
            serde_json::from_str(&request_json).map_err(js_error)?;
        let response = DefaultGrpcTransport::new(self.endpoint.clone())
            .search_catalog_models(catalog_proto::SearchCatalogModelsRequest {
                query: request.query,
                provider_id: request.provider_id.unwrap_or_default(),
                limit: request.limit.unwrap_or(10),
            })
            .await
            .map_err(js_error)?;

        serde_json::to_string(&CatalogSearchResponse::from(response)).map_err(js_error)
    }

    /// Suggest models.dev catalog matches and return a JSON response.
    #[wasm_bindgen(js_name = suggestCatalogMatchesJson)]
    pub async fn suggest_catalog_matches_json(
        &self,
        request_json: String,
    ) -> Result<String, JsValue> {
        let request: CatalogSuggestRequest =
            serde_json::from_str(&request_json).map_err(js_error)?;
        let response = DefaultGrpcTransport::new(self.endpoint.clone())
            .suggest_catalog_matches(catalog_proto::SuggestCatalogMatchesRequest {
                provider_key: request.provider_key,
                model_key: request.model_key,
                limit: request.limit.unwrap_or(5),
            })
            .await
            .map_err(js_error)?;

        serde_json::to_string(&CatalogSuggestResponse::from(response)).map_err(js_error)
    }

    /// Search session templates and return a JSON response.
    #[wasm_bindgen(js_name = searchSessionTemplatesJson)]
    pub async fn search_session_templates_json(
        &self,
        request_json: String,
    ) -> Result<String, JsValue> {
        let request: SessionTemplateSearchRequest =
            serde_json::from_str(&request_json).map_err(js_error)?;
        if request.query.trim().is_empty() {
            return serde_json::to_string(&SessionTemplateSearchResponse {
                request_id: request.request_id,
                options: Vec::new(),
                error: None,
            })
            .map_err(js_error);
        }

        let response = DefaultGrpcTransport::new(self.endpoint.clone())
            .search_session_templates(session_proto::SearchSessionTemplatesRequest {
                request_id: request.request_id,
                session_id: request.session_id,
                query: request.query,
                limit: request.limit.unwrap_or(20),
                kinds: request
                    .kinds
                    .into_iter()
                    .map(template_kind_to_proto)
                    .collect(),
                target: template_search_target_to_proto(request.target),
                cursor: request.cursor,
            })
            .await
            .map(SessionTemplateSearchResponse::from)
            .unwrap_or_else(|error| SessionTemplateSearchResponse {
                request_id: String::new(),
                options: Vec::new(),
                error: Some(error.to_string()),
            });

        serde_json::to_string(&response).map_err(js_error)
    }

    /// Advance the shared state stream and return the next JSON snapshot.
    #[wasm_bindgen(js_name = nextStateJson)]
    pub async fn next_state_json(&self) -> Result<String, JsValue> {
        self.snapshots
            .lock()
            .await
            .next()
            .await
            .ok_or_else(|| JsValue::from_str("client state stream closed"))
    }

    /// Advance the shared state stream, returning the current snapshot on timeout.
    #[wasm_bindgen(js_name = nextStateJsonWithTimeout)]
    pub async fn next_state_json_with_timeout(&self, timeout_ms: u32) -> Result<String, JsValue> {
        let next = self.next_state_json();
        let timeout = sleep_ms(timeout_ms);
        pin_mut!(next);
        pin_mut!(timeout);

        match select(next, timeout).await {
            Either::Left((snapshot, _)) => snapshot,
            Either::Right(((), pending_next)) => {
                drop(pending_next);
                Ok(self.latest_snapshot.borrow().clone())
            }
        }
    }
}

fn spawn_client_loop(
    mut runtime: crate::ClientStateStream,
    mut commands: mpsc::UnboundedReceiver<ClientCommandRequest>,
    snapshots: mpsc::UnboundedSender<String>,
    latest_snapshot: Rc<RefCell<String>>,
) {
    spawn_local(async move {
        loop {
            let event = runtime.next_or_dispatch(commands.next()).await;
            let ClientStateStreamEvent::Snapshot(snapshot) = event else {
                return;
            };

            let Ok(snapshot_json) = serde_json::to_string(snapshot.as_ref()) else {
                return;
            };

            latest_snapshot.replace(snapshot_json.clone());
            if snapshots.unbounded_send(snapshot_json).is_err() {
                return;
            }
        }
    });
}

async fn browser_sleep(delay: u64) {
    sleep_ms(delay.try_into().unwrap_or(u32::MAX)).await;
}

fn normalize_endpoint(endpoint: Option<String>) -> String {
    endpoint
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_owned()
}

fn js_error(error: impl ToString) -> JsValue {
    JsValue::from_str(&error.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSearchRequest {
    query: String,
    provider_id: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSuggestRequest {
    provider_key: String,
    model_key: String,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionTemplateSearchRequest {
    request_id: String,
    session_id: String,
    query: String,
    limit: Option<u32>,
    #[serde(default)]
    kinds: Vec<SessionTemplateKind>,
    #[serde(default)]
    target: SessionTemplateSearchTarget,
    cursor: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum SessionTemplateKind {
    File,
    Tool,
    Resource,
    Prompt,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum SessionTemplateSearchTarget {
    #[default]
    Resources,
    Prompts,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionTemplateSearchResponse {
    request_id: String,
    options: Vec<SessionTemplateOption>,
    error: Option<String>,
}

impl From<session_proto::SearchSessionTemplatesResponse> for SessionTemplateSearchResponse {
    fn from(response: session_proto::SearchSessionTemplatesResponse) -> Self {
        match response.result {
            Some(session_proto::search_session_templates_response::Result::Options(options)) => {
                Self {
                    request_id: response.request_id,
                    options: options
                        .options
                        .into_iter()
                        .map(SessionTemplateOption::from)
                        .collect(),
                    error: None,
                }
            }
            Some(session_proto::search_session_templates_response::Result::Error(error)) => Self {
                request_id: response.request_id,
                options: Vec::new(),
                error: Some(error.message),
            },
            None => Self {
                request_id: response.request_id,
                options: Vec::new(),
                error: Some("session resource search response missing result".to_owned()),
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionTemplateOption {
    kind: &'static str,
    label: String,
    detail: String,
    template: String,
    metadata: serde_json::Value,
}

impl From<session_proto::SessionTemplateOption> for SessionTemplateOption {
    fn from(option: session_proto::SessionTemplateOption) -> Self {
        Self {
            kind: match session_proto::SessionTemplateKind::try_from(option.kind).ok() {
                Some(session_proto::SessionTemplateKind::File) => "file",
                Some(session_proto::SessionTemplateKind::Tool) => "tool",
                Some(session_proto::SessionTemplateKind::Prompt) => "prompt",
                _ => "unknown",
            },
            label: option.label,
            detail: option.detail,
            template: option.template,
            metadata: serde_json::from_str(&option.metadata_json)
                .unwrap_or(serde_json::Value::Null),
        }
    }
}

fn template_kind_to_proto(kind: SessionTemplateKind) -> i32 {
    match kind {
        SessionTemplateKind::File => session_proto::SessionTemplateKind::File as i32,
        SessionTemplateKind::Tool => session_proto::SessionTemplateKind::Tool as i32,
        SessionTemplateKind::Resource => session_proto::SessionTemplateKind::Resource as i32,
        SessionTemplateKind::Prompt => session_proto::SessionTemplateKind::Prompt as i32,
    }
}

fn template_search_target_to_proto(target: SessionTemplateSearchTarget) -> i32 {
    match target {
        SessionTemplateSearchTarget::Resources => {
            session_proto::SessionTemplateSearchTarget::Resources as i32
        }
        SessionTemplateSearchTarget::Prompts => {
            session_proto::SessionTemplateSearchTarget::Prompts as i32
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSearchResponse {
    models: Vec<CatalogModelSummary>,
    stale: bool,
}

impl From<catalog_proto::SearchCatalogModelsResponse> for CatalogSearchResponse {
    fn from(response: catalog_proto::SearchCatalogModelsResponse) -> Self {
        Self {
            models: response
                .models
                .into_iter()
                .map(CatalogModelSummary::from)
                .collect(),
            stale: response.stale,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSuggestResponse {
    matches: Vec<CatalogMatch>,
    stale: bool,
}

impl From<catalog_proto::SuggestCatalogMatchesResponse> for CatalogSuggestResponse {
    fn from(response: catalog_proto::SuggestCatalogMatchesResponse) -> Self {
        Self {
            matches: response
                .matches
                .into_iter()
                .map(CatalogMatch::from)
                .collect(),
            stale: response.stale,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CatalogModelRef {
    provider_id: String,
    model_id: String,
}

impl From<catalog_proto::CatalogModelRef> for CatalogModelRef {
    fn from(model: catalog_proto::CatalogModelRef) -> Self {
        Self {
            provider_id: model.provider_id,
            model_id: model.model_id,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CatalogModelSummary {
    model: Option<CatalogModelRef>,
    provider_name: String,
    model_name: String,
    family: Option<String>,
    status: Option<String>,
    limits: Option<CatalogModelLimitsSummary>,
    cost: Option<CatalogModelCostSummary>,
    capabilities: Option<CatalogModelCapabilitySummary>,
    reasoning: Option<CatalogModelReasoningSummary>,
}

impl From<catalog_proto::CatalogModelSummary> for CatalogModelSummary {
    fn from(summary: catalog_proto::CatalogModelSummary) -> Self {
        Self {
            model: summary.model.map(CatalogModelRef::from),
            provider_name: summary.provider_name,
            model_name: summary.model_name,
            family: summary.family,
            status: summary.status,
            limits: summary.limits.map(CatalogModelLimitsSummary::from),
            cost: summary.cost.map(CatalogModelCostSummary::from),
            capabilities: summary
                .capabilities
                .map(CatalogModelCapabilitySummary::from),
            reasoning: summary.reasoning.map(CatalogModelReasoningSummary::from),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CatalogModelLimitsSummary {
    context: Option<u32>,
    output: Option<u32>,
}

impl From<catalog_proto::CatalogModelLimitsSummary> for CatalogModelLimitsSummary {
    fn from(limits: catalog_proto::CatalogModelLimitsSummary) -> Self {
        Self {
            context: limits.context,
            output: limits.output,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CatalogModelCostSummary {
    input_per_million_tokens: Option<f64>,
    output_per_million_tokens: Option<f64>,
    cache_read_per_million_tokens: Option<f64>,
    cache_write_per_million_tokens: Option<f64>,
    image_input_per_million_tokens: Option<f64>,
    image_input_per_image: Option<f64>,
    currency: String,
}

impl From<catalog_proto::CatalogModelCostSummary> for CatalogModelCostSummary {
    fn from(cost: catalog_proto::CatalogModelCostSummary) -> Self {
        Self {
            input_per_million_tokens: cost.input_per_million_tokens,
            output_per_million_tokens: cost.output_per_million_tokens,
            cache_read_per_million_tokens: cost.cache_read_per_million_tokens,
            cache_write_per_million_tokens: cost.cache_write_per_million_tokens,
            image_input_per_million_tokens: cost.image_input_per_million_tokens,
            image_input_per_image: cost.image_input_per_image,
            currency: cost.currency,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CatalogModelCapabilitySummary {
    tools: bool,
    image: bool,
    audio: bool,
}

impl From<catalog_proto::CatalogModelCapabilitySummary> for CatalogModelCapabilitySummary {
    fn from(capabilities: catalog_proto::CatalogModelCapabilitySummary) -> Self {
        Self {
            tools: capabilities.tools,
            image: capabilities.image,
            audio: capabilities.audio,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CatalogModelReasoningSummary {
    supported: bool,
    effort: Option<String>,
    efforts: Vec<String>,
}

impl From<catalog_proto::CatalogModelReasoningSummary> for CatalogModelReasoningSummary {
    fn from(reasoning: catalog_proto::CatalogModelReasoningSummary) -> Self {
        Self {
            supported: reasoning.supported,
            effort: reasoning.effort,
            efforts: reasoning.efforts,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogMatch {
    model: Option<CatalogModelRef>,
    provider_name: String,
    model_name: String,
    confidence: String,
    reasons: Vec<String>,
    upstream_model: Option<CatalogModelRef>,
}

impl From<catalog_proto::CatalogMatch> for CatalogMatch {
    fn from(catalog_match: catalog_proto::CatalogMatch) -> Self {
        Self {
            model: catalog_match.model.map(CatalogModelRef::from),
            provider_name: catalog_match.provider_name,
            model_name: catalog_match.model_name,
            confidence: catalog_proto::CatalogMatchConfidence::try_from(catalog_match.confidence)
                .map(catalog_match_confidence_name)
                .unwrap_or("unknown")
                .to_owned(),
            reasons: catalog_match.reasons,
            upstream_model: catalog_match.upstream_model.map(CatalogModelRef::from),
        }
    }
}

fn catalog_match_confidence_name(
    confidence: catalog_proto::CatalogMatchConfidence,
) -> &'static str {
    match confidence {
        catalog_proto::CatalogMatchConfidence::Unspecified => "unspecified",
        catalog_proto::CatalogMatchConfidence::Exact => "exact",
        catalog_proto::CatalogMatchConfidence::High => "high",
        catalog_proto::CatalogMatchConfidence::Suggested => "suggested",
    }
}

#[cfg(test)]
mod tests {
    use super::WaltrClient;

    #[test]
    fn defaults_to_same_origin_endpoint() {
        let client = WaltrClient::for_current_origin();

        assert_eq!(client.endpoint(), "");
    }

    #[test]
    fn trims_custom_endpoint() {
        let client = WaltrClient::new(Some(" http://127.0.0.1:8080/ ".to_owned()));

        assert_eq!(client.endpoint(), "http://127.0.0.1:8080");
    }
}
