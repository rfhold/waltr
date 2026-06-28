//! Models.dev catalog loading and gRPC service behavior.

mod models_dev;

use crate::config::{ConfigError, ConfigPatch, ConfigStore, FilesystemConfigStore};
use waltr::grpc::waltr::{catalog::v1 as catalog_proto, config::v1 as config_proto};
use std::{
    fmt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";
const ENV_MODELS_DEV_URL: &str = "WALTR_MODELS_DEV_URL";
const ENV_MODELS_DEV_PATH: &str = "WALTR_MODELS_DEV_PATH";
const ENV_MODELS_DEV_CACHE_PATH: &str = "WALTR_MODELS_DEV_CACHE_PATH";
const CACHE_RELATIVE_PATH: &[&str] = &[".local", "share", "waltr", "cache", "models-dev-api.json"];
const CATALOG_FRESHNESS: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// Loaded catalog with freshness metadata.
#[derive(Clone, Debug)]
pub struct LoadedCatalog {
    catalog: Arc<models_dev::ModelCatalog>,
    stale: bool,
}

impl LoadedCatalog {
    #[must_use]
    fn catalog(&self) -> &models_dev::ModelCatalog {
        &self.catalog
    }

    #[must_use]
    fn stale(&self) -> bool {
        self.stale
    }
}

#[derive(Clone, Debug)]
struct CachedCatalog {
    loaded: LoadedCatalog,
    loaded_at: Instant,
}

/// On-demand models.dev catalog loader with cache fallback.
#[derive(Clone, Debug)]
pub struct CatalogLoader {
    source: CatalogSource,
    cache_path: PathBuf,
    client: reqwest::Client,
    cached: Arc<Mutex<Option<CachedCatalog>>>,
}

impl CatalogLoader {
    /// Create a loader from `WALTR_MODELS_DEV_PATH`, `WALTR_MODELS_DEV_URL`, or defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let source = std::env::var_os(ENV_MODELS_DEV_PATH)
            .map(PathBuf::from)
            .map(CatalogSource::Path)
            .unwrap_or_else(|| {
                let url = std::env::var(ENV_MODELS_DEV_URL)
                    .unwrap_or_else(|_| DEFAULT_MODELS_DEV_URL.to_owned());
                CatalogSource::Url(url)
            });

        Self::new(source, default_cache_path())
    }

    /// Create a loader backed by a local fixture path.
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self::new(CatalogSource::Path(path.into()), default_cache_path())
    }

    #[must_use]
    fn new(source: CatalogSource, cache_path: PathBuf) -> Self {
        Self {
            source,
            cache_path,
            client: reqwest::Client::new(),
            cached: Arc::new(Mutex::new(None)),
        }
    }

    /// Load the catalog, refreshing stale in-memory entries on demand.
    pub async fn load(&self) -> Result<LoadedCatalog, CatalogLoadError> {
        let mut cached = self.cached.lock().await;
        if let Some(cached_catalog) = cached.as_ref()
            && cached_catalog.loaded_at.elapsed() < CATALOG_FRESHNESS
        {
            return Ok(cached_catalog.loaded.clone());
        }

        let loaded = match &self.source {
            CatalogSource::Path(path) => self.load_path(path).await?,
            CatalogSource::Url(url) => self.load_url(url).await?,
        };
        *cached = Some(CachedCatalog {
            loaded: loaded.clone(),
            loaded_at: Instant::now(),
        });

        Ok(loaded)
    }

    async fn load_path(&self, path: &PathBuf) -> Result<LoadedCatalog, CatalogLoadError> {
        let data = tokio::fs::read_to_string(path).await.map_err(|error| {
            CatalogLoadError::new(format!(
                "failed to read catalog fixture '{}': {error}",
                path.display()
            ))
        })?;
        parse_catalog(data, false)
    }

    async fn load_url(&self, url: &str) -> Result<LoadedCatalog, CatalogLoadError> {
        match self.fetch_and_cache(url).await {
            Ok(loaded) => Ok(loaded),
            Err(fetch_error) => match self.load_cached_file().await {
                Ok(Some(loaded)) => Ok(loaded),
                Ok(None) => Err(fetch_error),
                Err(cache_error) => Err(CatalogLoadError::new(format!(
                    "{fetch_error}; cache fallback failed: {cache_error}"
                ))),
            },
        }
    }

    async fn fetch_and_cache(&self, url: &str) -> Result<LoadedCatalog, CatalogLoadError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| {
                CatalogLoadError::new(format!("failed to fetch models.dev catalog: {error}"))
            })?;
        let data = response.text().await.map_err(|error| {
            CatalogLoadError::new(format!(
                "failed to read models.dev catalog response: {error}"
            ))
        })?;
        let loaded = parse_catalog(data.clone(), false)?;
        if let Err(error) = self.write_cache(&data).await {
            tracing::warn!(%error, path = %self.cache_path.display(), "failed to write catalog cache");
        }
        Ok(loaded)
    }

    async fn load_cached_file(&self) -> Result<Option<LoadedCatalog>, CatalogLoadError> {
        match tokio::fs::read_to_string(&self.cache_path).await {
            Ok(data) => parse_catalog(data, true).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(CatalogLoadError::new(format!(
                "failed to read catalog cache '{}': {error}",
                self.cache_path.display()
            ))),
        }
    }

    async fn write_cache(&self, data: &str) -> Result<(), CatalogLoadError> {
        if let Some(parent) = self.cache_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                CatalogLoadError::new(format!(
                    "failed to create catalog cache directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }

        let tmp = self.cache_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, data).await.map_err(|error| {
            CatalogLoadError::new(format!(
                "failed to write catalog cache '{}': {error}",
                tmp.display()
            ))
        })?;
        tokio::fs::rename(&tmp, &self.cache_path)
            .await
            .map_err(|error| {
                CatalogLoadError::new(format!(
                    "failed to replace catalog cache '{}': {error}",
                    self.cache_path.display()
                ))
            })
    }
}

fn parse_catalog(data: String, stale: bool) -> Result<LoadedCatalog, CatalogLoadError> {
    let catalog = models_dev::ModelCatalog::from_json_str(&data)
        .map_err(|error| CatalogLoadError::new(format!("failed to parse catalog: {error}")))?;
    Ok(LoadedCatalog {
        catalog: Arc::new(catalog),
        stale,
    })
}

fn default_cache_path() -> PathBuf {
    if let Some(path) = std::env::var_os(ENV_MODELS_DEV_CACHE_PATH) {
        return PathBuf::from(path);
    }

    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    CACHE_RELATIVE_PATH
        .iter()
        .fold(PathBuf::from(home), |path, segment| path.join(segment))
}

#[derive(Clone, Debug)]
enum CatalogSource {
    Url(String),
    Path(PathBuf),
}

/// Catalog load failure.
#[derive(Debug)]
pub struct CatalogLoadError {
    message: String,
}

impl CatalogLoadError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl fmt::Display for CatalogLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CatalogLoadError {}

/// gRPC catalog service backed by the shared configuration store.
#[derive(Debug)]
pub struct CatalogService<S = FilesystemConfigStore> {
    store: S,
    loader: CatalogLoader,
}

impl<S> CatalogService<S>
where
    S: ConfigStore,
{
    /// Create a catalog service from a config store and catalog loader.
    #[must_use]
    pub fn new(store: S, loader: CatalogLoader) -> Arc<Self> {
        Arc::new(Self { store, loader })
    }
}

#[tonic::async_trait]
impl<S> catalog_proto::catalog_service_server::CatalogService for CatalogService<S>
where
    S: ConfigStore,
{
    async fn search_catalog_models(
        &self,
        request: Request<catalog_proto::SearchCatalogModelsRequest>,
    ) -> Result<Response<catalog_proto::SearchCatalogModelsResponse>, Status> {
        let request = request.into_inner();
        let loaded = self.load_catalog().await?;
        let limit = request_limit(request.limit);
        let search_limit = if request.provider_id.trim().is_empty() {
            limit
        } else {
            usize::MAX
        };
        let models = loaded
            .catalog()
            .search(&request.query, search_limit)
            .into_iter()
            .filter(|catalog_match| {
                request.provider_id.trim().is_empty()
                    || catalog_match.model.provider_id == request.provider_id
            })
            .take(limit)
            .filter_map(|catalog_match| catalog_summary(loaded.catalog(), &catalog_match.model))
            .collect();

        Ok(Response::new(catalog_proto::SearchCatalogModelsResponse {
            models,
            stale: loaded.stale(),
        }))
    }

    async fn suggest_catalog_matches(
        &self,
        request: Request<catalog_proto::SuggestCatalogMatchesRequest>,
    ) -> Result<Response<catalog_proto::SuggestCatalogMatchesResponse>, Status> {
        let request = request.into_inner();
        let config = self.load_config().await?;
        let provider_url = config
            .providers
            .get(&request.provider_key)
            .map(|provider| provider.url.as_str());
        let loaded = self.load_catalog().await?;
        let matches = loaded
            .catalog()
            .suggest_matches(
                non_empty(request.provider_key.as_str()),
                provider_url,
                &request.model_key,
                request_limit(request.limit),
            )
            .into_iter()
            .filter_map(|catalog_match| proto_catalog_match(loaded.catalog(), catalog_match))
            .collect();

        Ok(Response::new(
            catalog_proto::SuggestCatalogMatchesResponse {
                matches,
                stale: loaded.stale(),
            },
        ))
    }

    async fn get_model_catalog_drift(
        &self,
        request: Request<catalog_proto::GetModelCatalogDriftRequest>,
    ) -> Result<Response<catalog_proto::GetModelCatalogDriftResponse>, Status> {
        let request = request.into_inner();
        let config = self.load_config().await?;
        let (provider, model) =
            configured_model(&config, &request.provider_key, &request.model_key)?;
        let loaded = self.load_catalog().await?;
        let catalog_match = resolve_catalog_match(
            loaded.catalog(),
            &request.provider_key,
            provider,
            &request.model_key,
            model,
            None,
            config_proto::CatalogMatchSource::Auto,
        )?;
        let fields = catalog_match
            .as_ref()
            .and_then(|catalog_match| {
                lookup_catalog_model(loaded.catalog(), catalog_match).map(|catalog_model| {
                    let facts = configured_facts(model);
                    catalog_model
                        .compare_configured_facts(&facts)
                        .into_iter()
                        .map(proto_drift_field)
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();

        Ok(Response::new(catalog_proto::GetModelCatalogDriftResponse {
            fields,
            stale: loaded.stale(),
            catalog_match,
        }))
    }

    async fn set_model_catalog_match(
        &self,
        request: Request<catalog_proto::SetModelCatalogMatchRequest>,
    ) -> Result<Response<catalog_proto::SetModelCatalogMatchResponse>, Status> {
        let request = request.into_inner();
        let mut config = self.load_config().await?;
        let provider = config
            .providers
            .get_mut(&request.provider_key)
            .ok_or_else(|| provider_not_found(&request.provider_key))?;
        let model = provider
            .models
            .get_mut(&request.model_key)
            .ok_or_else(|| model_not_found(&request.provider_key, &request.model_key))?;

        if request.clear_match {
            model.catalog_match = None;
        } else {
            let catalog_match = request.catalog_match.ok_or_else(|| {
                Status::invalid_argument("catalog_match is required unless clear_match is true")
            })?;
            model.catalog_match = Some(normalize_catalog_match(
                catalog_match,
                config_proto::CatalogMatchSource::Manual,
            )?);
        }

        let config = self.patch_providers(config, &request.expected_etag).await?;
        Ok(Response::new(catalog_proto::SetModelCatalogMatchResponse {
            config: Some(config),
        }))
    }

    async fn apply_catalog_model_defaults(
        &self,
        request: Request<catalog_proto::ApplyCatalogModelDefaultsRequest>,
    ) -> Result<Response<catalog_proto::ApplyCatalogModelDefaultsResponse>, Status> {
        let request = request.into_inner();
        let mut config = self.load_config().await?;
        let loaded = self.load_catalog().await?;
        let (provider, model) =
            configured_model(&config, &request.provider_key, &request.model_key)?;
        let catalog_match = resolve_catalog_match(
            loaded.catalog(),
            &request.provider_key,
            provider,
            &request.model_key,
            model,
            request.catalog_match,
            config_proto::CatalogMatchSource::Applied,
        )?
        .ok_or_else(|| Status::not_found("no high-confidence catalog match found"))?;
        let catalog_model = lookup_catalog_model(loaded.catalog(), &catalog_match)
            .ok_or_else(|| Status::not_found("catalog model was not found"))?;
        let fields = RequestedDefaultFields::from_proto(&request.fields);

        let provider = config
            .providers
            .get_mut(&request.provider_key)
            .ok_or_else(|| provider_not_found(&request.provider_key))?;
        let model = provider
            .models
            .get_mut(&request.model_key)
            .ok_or_else(|| model_not_found(&request.provider_key, &request.model_key))?;
        apply_defaults(model, catalog_model, &catalog_match, fields);

        let config = self.patch_providers(config, &request.expected_etag).await?;
        Ok(Response::new(
            catalog_proto::ApplyCatalogModelDefaultsResponse {
                config: Some(config),
            },
        ))
    }
}

impl<S> CatalogService<S>
where
    S: ConfigStore,
{
    async fn load_catalog(&self) -> Result<LoadedCatalog, Status> {
        self.loader.load().await.map_err(|error| {
            tracing::warn!(%error, "catalog load failed");
            Status::unavailable(error.to_string())
        })
    }

    async fn load_config(&self) -> Result<config_proto::Configuration, Status> {
        self.store.load().await.map_err(|error| {
            tracing::warn!(%error, "catalog config load failed");
            config_error_to_status(&error)
        })
    }

    async fn patch_providers(
        &self,
        config: config_proto::Configuration,
        expected_etag: &str,
    ) -> Result<config_proto::Configuration, Status> {
        self.store
            .patch(
                ConfigPatch {
                    providers: Some(config.providers),
                    mcp_servers: None,
                    runtime_defaults: None,
                },
                expected_etag,
            )
            .await
            .map_err(|error| {
                tracing::warn!(%error, "catalog config patch failed");
                config_error_to_status(&error)
            })
    }
}

#[derive(Clone, Copy, Debug)]
struct RequestedDefaultFields {
    name: bool,
    model_type: bool,
    limits: bool,
    capabilities: bool,
    cost: bool,
}

impl RequestedDefaultFields {
    fn from_proto(fields: &[i32]) -> Self {
        if fields.is_empty() {
            return Self {
                name: true,
                model_type: true,
                limits: true,
                capabilities: true,
                cost: true,
            };
        }

        let mut requested = Self {
            name: false,
            model_type: false,
            limits: false,
            capabilities: false,
            cost: false,
        };
        for field in fields {
            match catalog_proto::CatalogDefaultField::try_from(*field).ok() {
                Some(catalog_proto::CatalogDefaultField::Name) => requested.name = true,
                Some(catalog_proto::CatalogDefaultField::Type) => requested.model_type = true,
                Some(catalog_proto::CatalogDefaultField::Limits) => requested.limits = true,
                Some(catalog_proto::CatalogDefaultField::Capabilities) => {
                    requested.capabilities = true;
                }
                Some(catalog_proto::CatalogDefaultField::Cost) => requested.cost = true,
                Some(catalog_proto::CatalogDefaultField::Unspecified) | None => {}
            }
        }
        requested
    }
}

fn apply_defaults(
    model: &mut config_proto::Model,
    catalog_model: &models_dev::CatalogModel,
    catalog_match: &config_proto::ModelCatalogMatch,
    fields: RequestedDefaultFields,
) {
    model.catalog_match = Some(catalog_match.clone());

    if fields.name && model.name.trim().is_empty() {
        model.name.clone_from(&catalog_model.name);
    }

    if fields.model_type && model_type_is_blank(model.r#type) {
        model.r#type = config_proto::ModelType::Llm as i32;
    }

    if fields.limits {
        let limits = model
            .limits
            .get_or_insert_with(config_proto::ModelLimits::default);
        if limits.context.is_none() {
            limits.context = u64_to_u32(catalog_model.limits.context);
        }
        if limits.output.is_none() {
            limits.output = u64_to_u32(catalog_model.limits.output);
        }
    }

    if fields.capabilities && model.capabilities.is_none() {
        model.capabilities = Some(config_proto::ModelCapabilities {
            tools: catalog_model.capabilities.tool_call.unwrap_or(false),
            image: catalog_supports_image(catalog_model),
        });
    }

    if fields.cost {
        model.cost = Some(model_cost_from_catalog(catalog_model));
    }

    let reasoning_efforts = catalog_reasoning_efforts(catalog_model);
    if !reasoning_efforts.is_empty() {
        let options = model
            .options
            .get_or_insert_with(config_proto::ModelOptions::default);
        if options.reasoning_efforts.is_empty() {
            options.reasoning_efforts = reasoning_efforts;
        }
    }
}

fn catalog_reasoning_efforts(catalog_model: &models_dev::CatalogModel) -> Vec<String> {
    let mut efforts = catalog_model
        .reasoning_options
        .iter()
        .flat_map(|option| match option {
            models_dev::CatalogReasoningOption::Effort { values } => values.as_slice(),
            _ => &[],
        })
        .filter_map(|value| match value {
            models_dev::CatalogReasoningEffortValue::String(value) if !value.is_empty() => {
                Some(value.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    efforts.sort();
    efforts.dedup();
    efforts
}

fn model_type_is_blank(value: i32) -> bool {
    matches!(
        config_proto::ModelType::try_from(value),
        Ok(config_proto::ModelType::Unspecified) | Err(_)
    )
}

fn resolve_catalog_match(
    catalog: &models_dev::ModelCatalog,
    provider_key: &str,
    provider: &config_proto::Provider,
    model_key: &str,
    model: &config_proto::Model,
    selected: Option<config_proto::ModelCatalogMatch>,
    default_source: config_proto::CatalogMatchSource,
) -> Result<Option<config_proto::ModelCatalogMatch>, Status> {
    if let Some(catalog_match) = selected.or_else(|| model.catalog_match.clone()) {
        return normalize_catalog_match(catalog_match, default_source).map(Some);
    }

    let Some(suggestion) = catalog
        .suggest_matches(
            Some(provider_key),
            Some(provider.url.as_str()),
            model_key,
            1,
        )
        .into_iter()
        .find(|suggestion| high_confidence(suggestion.confidence))
    else {
        return Ok(None);
    };

    let upstream = derive_upstream_model_ref(catalog, &suggestion.model);
    Ok(Some(config_proto::ModelCatalogMatch {
        provider_id: suggestion.model.provider_id,
        model_id: suggestion.model.model_id,
        upstream_provider_id: upstream
            .as_ref()
            .map(|upstream| upstream.provider_id.clone()),
        upstream_model_id: upstream.map(|upstream| upstream.model_id),
        source: default_source as i32,
    }))
}

fn normalize_catalog_match(
    mut catalog_match: config_proto::ModelCatalogMatch,
    default_source: config_proto::CatalogMatchSource,
) -> Result<config_proto::ModelCatalogMatch, Status> {
    catalog_match.provider_id = catalog_match.provider_id.trim().to_owned();
    catalog_match.model_id = catalog_match.model_id.trim().to_owned();
    catalog_match.upstream_provider_id = cleaned_optional(catalog_match.upstream_provider_id);
    catalog_match.upstream_model_id = cleaned_optional(catalog_match.upstream_model_id);

    if catalog_match.provider_id.is_empty() || catalog_match.model_id.is_empty() {
        return Err(Status::invalid_argument(
            "catalog_match provider_id and model_id are required",
        ));
    }

    if matches!(
        config_proto::CatalogMatchSource::try_from(catalog_match.source),
        Ok(config_proto::CatalogMatchSource::Unspecified) | Err(_)
    ) {
        catalog_match.source = default_source as i32;
    }

    Ok(catalog_match)
}

fn cleaned_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn lookup_catalog_model<'a>(
    catalog: &'a models_dev::ModelCatalog,
    catalog_match: &config_proto::ModelCatalogMatch,
) -> Option<&'a models_dev::CatalogModel> {
    let upstream_provider_id = catalog_match
        .upstream_provider_id
        .as_deref()
        .filter(|value| !value.is_empty());
    let upstream_model_id = catalog_match
        .upstream_model_id
        .as_deref()
        .filter(|value| !value.is_empty());

    if let (Some(provider_id), Some(model_id)) = (upstream_provider_id, upstream_model_id)
        && let Some(model) = catalog.model(provider_id, model_id)
    {
        return Some(model);
    }

    catalog.model(&catalog_match.provider_id, &catalog_match.model_id)
}

fn configured_model<'a>(
    config: &'a config_proto::Configuration,
    provider_key: &str,
    model_key: &str,
) -> Result<(&'a config_proto::Provider, &'a config_proto::Model), Status> {
    let provider = config
        .providers
        .get(provider_key)
        .ok_or_else(|| provider_not_found(provider_key))?;
    let model = provider
        .models
        .get(model_key)
        .ok_or_else(|| model_not_found(provider_key, model_key))?;
    Ok((provider, model))
}

fn configured_facts(model: &config_proto::Model) -> models_dev::ConfiguredModelFacts {
    let limits = model.limits.unwrap_or_default();
    let capabilities = model.capabilities;

    models_dev::ConfiguredModelFacts {
        limits: models_dev::CatalogLimits {
            context: limits.context.map(u64::from),
            input: None,
            output: limits.output.map(u64::from),
        },
        capabilities: models_dev::CatalogCapabilities {
            attachment: capabilities.map(|capabilities| capabilities.image),
            reasoning: None,
            tool_call: capabilities.map(|capabilities| capabilities.tools),
            structured_output: None,
            temperature: None,
        },
        cost: model.cost.as_ref().map(|cost| models_dev::CatalogCost {
            input: cost.input_per_million_tokens,
            output: cost.output_per_million_tokens,
            cache_read: cost.cache_read_per_million_tokens,
            cache_write: cost.cache_write_per_million_tokens,
            ..models_dev::CatalogCost::default()
        }),
        ..models_dev::ConfiguredModelFacts::default()
    }
}

fn model_cost_from_catalog(catalog_model: &models_dev::CatalogModel) -> config_proto::ModelCost {
    config_proto::ModelCost {
        input_per_million_tokens: catalog_model.cost.input,
        output_per_million_tokens: catalog_model.cost.output,
        cache_read_per_million_tokens: catalog_model.cost.cache_read,
        cache_write_per_million_tokens: catalog_model.cost.cache_write,
        image_input_per_million_tokens: None,
        image_input_per_image: None,
        currency: "USD".to_owned(),
    }
}

fn catalog_summary(
    catalog: &models_dev::ModelCatalog,
    model_ref: &models_dev::CatalogModelRef,
) -> Option<catalog_proto::CatalogModelSummary> {
    let provider = catalog.provider(&model_ref.provider_id)?;
    let model = provider.models.get(&model_ref.model_id)?;
    Some(catalog_proto::CatalogModelSummary {
        model: Some(catalog_proto::CatalogModelRef {
            provider_id: provider.id.clone(),
            model_id: model.id.clone(),
        }),
        provider_name: provider.name.clone(),
        model_name: model.name.clone(),
        family: model.family.clone(),
        status: model.status.clone(),
        limits: Some(catalog_proto::CatalogModelLimitsSummary {
            context: u64_to_u32(model.limits.context),
            output: u64_to_u32(model.limits.output),
        }),
        cost: Some(catalog_proto::CatalogModelCostSummary {
            input_per_million_tokens: model.cost.input,
            output_per_million_tokens: model.cost.output,
            currency: "USD".to_owned(),
            cache_read_per_million_tokens: model.cost.cache_read,
            cache_write_per_million_tokens: model.cost.cache_write,
            image_input_per_million_tokens: None,
            image_input_per_image: None,
        }),
        capabilities: Some(catalog_proto::CatalogModelCapabilitySummary {
            tools: model.capabilities.tool_call.unwrap_or(false),
            image: catalog_supports_image(model),
            audio: catalog_supports_audio(model),
        }),
        reasoning: Some(catalog_proto::CatalogModelReasoningSummary {
            supported: model.capabilities.reasoning.unwrap_or(false)
                || !model.reasoning_options.is_empty(),
            effort: catalog_reasoning_effort(model),
            efforts: catalog_reasoning_efforts(model),
        }),
    })
}

fn proto_catalog_match(
    catalog: &models_dev::ModelCatalog,
    catalog_match: models_dev::CatalogMatch,
) -> Option<catalog_proto::CatalogMatch> {
    let provider = catalog.provider(&catalog_match.model.provider_id)?;
    let model = provider.models.get(&catalog_match.model.model_id)?;
    let upstream_model = derive_upstream_model_ref(catalog, &catalog_match.model).map(|model| {
        catalog_proto::CatalogModelRef {
            provider_id: model.provider_id,
            model_id: model.model_id,
        }
    });

    Some(catalog_proto::CatalogMatch {
        model: Some(catalog_proto::CatalogModelRef {
            provider_id: provider.id.clone(),
            model_id: model.id.clone(),
        }),
        provider_name: provider.name.clone(),
        model_name: model.name.clone(),
        confidence: proto_confidence(catalog_match.confidence) as i32,
        reasons: vec![catalog_match.reason],
        upstream_model,
    })
}

fn derive_upstream_model_ref(
    catalog: &models_dev::ModelCatalog,
    model_ref: &models_dev::CatalogModelRef,
) -> Option<models_dev::CatalogModelRef> {
    let (provider_id, model_id) = model_ref.model_id.split_once('/')?;
    if provider_id.is_empty()
        || model_id.is_empty()
        || catalog.model(provider_id, model_id).is_none()
    {
        return None;
    }

    Some(models_dev::CatalogModelRef::new(provider_id, model_id))
}

fn proto_confidence(
    confidence: models_dev::CatalogMatchConfidence,
) -> catalog_proto::CatalogMatchConfidence {
    match confidence {
        models_dev::CatalogMatchConfidence::Exact => catalog_proto::CatalogMatchConfidence::Exact,
        models_dev::CatalogMatchConfidence::High => catalog_proto::CatalogMatchConfidence::High,
        models_dev::CatalogMatchConfidence::Suggested => {
            catalog_proto::CatalogMatchConfidence::Suggested
        }
    }
}

fn high_confidence(confidence: models_dev::CatalogMatchConfidence) -> bool {
    matches!(
        confidence,
        models_dev::CatalogMatchConfidence::Exact | models_dev::CatalogMatchConfidence::High
    )
}

fn proto_drift_field(drift: models_dev::CatalogDrift) -> catalog_proto::CatalogDriftField {
    catalog_proto::CatalogDriftField {
        field: drift_field_name(drift.field).to_owned(),
        configured: drift_value(drift.configured),
        catalog: drift_value(drift.catalog),
        severity: drift_severity(drift.field) as i32,
    }
}

fn drift_field_name(field: models_dev::CatalogDriftField) -> &'static str {
    match field {
        models_dev::CatalogDriftField::Status => "status",
        models_dev::CatalogDriftField::LimitContext => "limit_context",
        models_dev::CatalogDriftField::LimitInput => "limit_input",
        models_dev::CatalogDriftField::LimitOutput => "limit_output",
        models_dev::CatalogDriftField::CapabilityAttachment => "capability_attachment",
        models_dev::CatalogDriftField::CapabilityReasoning => "capability_reasoning",
        models_dev::CatalogDriftField::CapabilityToolCall => "capability_tool_call",
        models_dev::CatalogDriftField::CapabilityStructuredOutput => "capability_structured_output",
        models_dev::CatalogDriftField::CapabilityTemperature => "capability_temperature",
        models_dev::CatalogDriftField::ModalityInput => "modality_input",
        models_dev::CatalogDriftField::ModalityOutput => "modality_output",
        models_dev::CatalogDriftField::ReasoningOptions => "reasoning_options",
        models_dev::CatalogDriftField::ProviderShape => "provider_shape",
        models_dev::CatalogDriftField::CostInput => "cost_input",
        models_dev::CatalogDriftField::CostOutput => "cost_output",
        models_dev::CatalogDriftField::CostCacheRead => "cost_cache_read",
        models_dev::CatalogDriftField::CostCacheWrite => "cost_cache_write",
    }
}

fn drift_severity(field: models_dev::CatalogDriftField) -> catalog_proto::CatalogDriftSeverity {
    match field {
        models_dev::CatalogDriftField::LimitContext
        | models_dev::CatalogDriftField::LimitInput
        | models_dev::CatalogDriftField::LimitOutput
        | models_dev::CatalogDriftField::CapabilityAttachment
        | models_dev::CatalogDriftField::CapabilityReasoning
        | models_dev::CatalogDriftField::CapabilityToolCall
        | models_dev::CatalogDriftField::CapabilityStructuredOutput
        | models_dev::CatalogDriftField::CapabilityTemperature => {
            catalog_proto::CatalogDriftSeverity::Actionable
        }
        models_dev::CatalogDriftField::Status
        | models_dev::CatalogDriftField::ReasoningOptions
        | models_dev::CatalogDriftField::ProviderShape
        | models_dev::CatalogDriftField::CostInput
        | models_dev::CatalogDriftField::CostOutput
        | models_dev::CatalogDriftField::CostCacheRead
        | models_dev::CatalogDriftField::CostCacheWrite => {
            catalog_proto::CatalogDriftSeverity::Warning
        }
        models_dev::CatalogDriftField::ModalityInput
        | models_dev::CatalogDriftField::ModalityOutput => {
            catalog_proto::CatalogDriftSeverity::Info
        }
    }
}

fn drift_value(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value,
        value => value.to_string(),
    }
}

fn catalog_supports_image(model: &models_dev::CatalogModel) -> bool {
    model.capabilities.attachment.unwrap_or(false)
        || model
            .modalities
            .input
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("image"))
}

fn catalog_supports_audio(model: &models_dev::CatalogModel) -> bool {
    model
        .modalities
        .input
        .iter()
        .chain(model.modalities.output.iter())
        .any(|modality| modality.eq_ignore_ascii_case("audio"))
}

fn catalog_reasoning_effort(model: &models_dev::CatalogModel) -> Option<String> {
    model
        .reasoning_options
        .iter()
        .find_map(|option| match option {
            models_dev::CatalogReasoningOption::Effort { values } => {
                let values = values
                    .iter()
                    .filter_map(|value| match value {
                        models_dev::CatalogReasoningEffortValue::String(value) => {
                            Some(value.clone())
                        }
                        models_dev::CatalogReasoningEffortValue::Other(_) => None,
                    })
                    .collect::<Vec<_>>();
                (!values.is_empty()).then(|| values.join(","))
            }
            _ => None,
        })
}

fn u64_to_u32(value: Option<u64>) -> Option<u32> {
    value.and_then(|value| u32::try_from(value).ok())
}

fn request_limit(limit: u32) -> usize {
    if limit == 0 {
        DEFAULT_LIMIT
    } else {
        usize::try_from(limit).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn provider_not_found(provider_key: &str) -> Status {
    Status::not_found(format!("provider '{provider_key}' was not found"))
}

fn model_not_found(provider_key: &str, model_key: &str) -> Status {
    Status::not_found(format!(
        "model '{model_key}' was not found in provider '{provider_key}'"
    ))
}

fn config_error_to_status(error: &ConfigError) -> Status {
    if error.is_etag_mismatch() {
        Status::failed_precondition(error.to_string())
    } else if error.is_invalid() {
        Status::invalid_argument(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}
