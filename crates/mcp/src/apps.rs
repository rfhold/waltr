//! SEP-1865 (2026-01-26) wire helpers. These do not implement a browser sandbox,
//! authorization, CSP headers, or the iframe lifecycle state machine.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Extension identifier used in core MCP per-request capabilities.
pub const UI_EXTENSION: &str = "io.modelcontextprotocol/ui";
/// The only UI resource MIME type supported by this revision.
pub const UI_MIME_TYPE: &str = "text/html;profile=mcp-app";
/// Iframe protocol revision, independent of the core MCP revision.
pub const UI_PROTOCOL_VERSION: &str = "2026-01-26";
/// Default maximum decoded HTML size (one MiB).
pub const DEFAULT_MAX_HTML_BYTES: usize = 1024 * 1024;
/// View-to-host initialization method.
pub const UI_INITIALIZE: &str = "ui/initialize";
/// View initialization completion notification.
pub const UI_INITIALIZED: &str = "ui/notifications/initialized";
/// Complete tool input notification.
pub const UI_TOOL_INPUT: &str = "ui/notifications/tool-input";
/// Partial tool input notification.
pub const UI_TOOL_INPUT_PARTIAL: &str = "ui/notifications/tool-input-partial";
/// Tool result notification.
pub const UI_TOOL_RESULT: &str = "ui/notifications/tool-result";
/// Pre-teardown request (not a notification).
pub const UI_RESOURCE_TEARDOWN: &str = "ui/resource-teardown";

/// Failure to interpret Apps data; ordinary MCP parsing remains independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppsError {
    /// An Apps field has an invalid shape or value.
    Metadata(String),
    /// URI is not a UI URI, or does not exactly match the requested URI.
    Uri,
    /// Resource MIME type is missing or unsupported.
    MimeType,
    /// Expected one resource with exactly one string text/blob field.
    Content,
    /// Decoded content exceeds the caller's byte limit.
    TooLarge,
    /// Blob is not standard base64.
    Base64,
    /// Blob does not decode to UTF-8.
    Utf8,
    /// Content is not an HTML5 document envelope.
    HtmlDocument,
}

impl std::fmt::Display for AppsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Metadata(message) => write!(f, "invalid Apps metadata: {message}"),
            Self::Uri => f.write_str("UI resource URI is invalid or mismatched"),
            Self::MimeType => f.write_str("unsupported UI resource MIME type"),
            Self::Content => f.write_str("expected one UI resource with exactly one text or blob"),
            Self::TooLarge => f.write_str("UI resource exceeds decoded byte limit"),
            Self::Base64 => f.write_str("UI resource blob is not standard base64"),
            Self::Utf8 => f.write_str("UI resource blob is not UTF-8"),
            Self::HtmlDocument => f.write_str("UI resource is not an HTML5 document envelope"),
        }
    }
}

impl std::error::Error for AppsError {}

/// A resource read failure, retaining the request or Apps decoding error.
#[derive(Debug)]
pub enum ReadAppResourceError {
    /// The MCP request failed (including cancellation).
    Request(crate::Error),
    /// The response was not a valid Apps resource within the supplied limit.
    Apps(AppsError),
}

impl std::fmt::Display for ReadAppResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) => write!(f, "{error}"),
            Self::Apps(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ReadAppResourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Request(error) => error,
            Self::Apps(error) => error,
        })
    }
}

/// Apps interpretation of a tool declaration, borrowing the unchanged core definition.
/// No server identity, authorization, or resource-declaration equality is implied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiToolDeclaration<'a> {
    /// Original tool including raw metadata and schema.
    pub tool: &'a crate::McpToolDefinition,
    /// Validated UI association and audience visibility, including defaults.
    pub ui: UiToolMeta,
}

impl<'a> TryFrom<&'a crate::McpToolDefinition> for UiToolDeclaration<'a> {
    type Error = AppsError;

    fn try_from(tool: &'a crate::McpToolDefinition) -> Result<Self, Self::Error> {
        Ok(Self {
            tool,
            ui: tool_meta(tool.meta.as_ref())?,
        })
    }
}

/// Apps interpretation of an explicitly selected UI resource declaration.
/// Declaration metadata is not merged with, or required to equal, read metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiResourceDeclaration<'a> {
    /// Original resource including raw metadata.
    pub resource: &'a crate::McpResourceDefinition,
    /// Validated declared UI metadata, not a host policy or permission grant.
    pub ui: UiResourceMeta,
}

impl<'a> TryFrom<&'a crate::McpResourceDefinition> for UiResourceDeclaration<'a> {
    type Error = AppsError;

    fn try_from(resource: &'a crate::McpResourceDefinition) -> Result<Self, Self::Error> {
        validate_uri(&resource.uri)?;
        if resource.mime_type.as_deref() != Some(UI_MIME_TYPE) {
            return Err(AppsError::MimeType);
        }
        Ok(Self {
            resource,
            ui: resource_meta(resource.meta.as_ref())?,
        })
    }
}

/// Who may see and invoke a tool. Enforcement belongs to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiVisibility {
    /// Agent-visible and agent-callable.
    Model,
    /// Callable by an app on the originating server connection.
    App,
}

fn default_visibility() -> Vec<UiVisibility> {
    vec![UiVisibility::Model, UiVisibility::App]
}

/// The nested `_meta.ui` of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiToolMeta {
    /// Associated resource URI, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_uri: Option<String>,
    /// Defaults to both model and app when omitted; an explicit empty list hides both.
    #[serde(default = "default_visibility")]
    pub visibility: Vec<UiVisibility>,
    /// Unknown nested metadata retained for round trips.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for UiToolMeta {
    fn default() -> Self {
        Self {
            resource_uri: None,
            visibility: default_visibility(),
            extra: Map::new(),
        }
    }
}

impl UiToolMeta {
    /// Whether a given audience can invoke the tool.
    #[must_use]
    pub fn visible_to(&self, audience: UiVisibility) -> bool {
        self.visibility.contains(&audience)
    }
}

/// Declared CSP sources, not CSP headers. Omitted lists have restrictive defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCsp {
    /// Origins for fetch/XHR/WebSocket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_domains: Option<Vec<String>>,
    /// Origins for scripts, styles, images, fonts and media.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_domains: Option<Vec<String>>,
    /// Origins for nested frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_domains: Option<Vec<String>>,
    /// Allowed document base origins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_uri_domains: Option<Vec<String>>,
    /// Unknown future fields; hosts must decide whether they can honor them.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Object-shaped feature marker, retaining future settings.
pub type UiFeature = Map<String, Value>;

/// Requested permissions, not grants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPermissions {
    /// Camera permission request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<UiFeature>,
    /// Microphone permission request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<UiFeature>,
    /// Geolocation permission request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geolocation: Option<UiFeature>,
    /// Clipboard write permission request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_write: Option<UiFeature>,
    /// Unknown future permissions, never implicitly granted by these helpers.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The nested `_meta.ui` of a UI resource or resource content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiResourceMeta {
    /// Declared external origins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csp: Option<UiCsp>,
    /// Browser permissions requested by the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<UiPermissions>,
    /// Host-dependent sandbox domain hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Visual border preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefers_border: Option<bool>,
    /// Unknown nested metadata retained for round trips.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Advertised host support, including the required MIME type list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCapability {
    /// Supported resource MIME types; required even when empty.
    pub mime_types: Vec<String>,
    /// Unknown capability settings retained for round trips.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl UiCapability {
    /// Advertise the HTML type defined by SEP-1865.
    #[must_use]
    pub fn html() -> Self {
        Self {
            mime_types: vec![UI_MIME_TYPE.to_owned()],
            extra: Map::new(),
        }
    }

    /// Whether this capability includes the currently supported HTML type.
    #[must_use]
    pub fn supports_html(&self) -> bool {
        self.mime_types.iter().any(|mime| mime == UI_MIME_TYPE)
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, AppsError> {
    serde_json::from_value(value.clone()).map_err(|error| AppsError::Metadata(error.to_string()))
}

fn object(value: &Value) -> Result<&Map<String, Value>, AppsError> {
    value
        .as_object()
        .ok_or_else(|| AppsError::Metadata("expected object".into()))
}

fn reject_nulls(value: &Value, keys: &[&str]) -> Result<(), AppsError> {
    for key in keys {
        if value.get(*key).is_some_and(Value::is_null) {
            return Err(AppsError::Metadata(format!("{key} cannot be null")));
        }
    }
    Ok(())
}

fn validate_uri(uri: &str) -> Result<(), AppsError> {
    if !uri.starts_with("ui://")
        || uri.len() <= 5
        || uri.chars().any(char::is_whitespace)
        || url::Url::parse(uri).is_err()
    {
        return Err(AppsError::Uri);
    }
    Ok(())
}

/// Decode `_meta` without modifying it. Nested resourceUri wins; the deprecated
/// flat key is used only when the nested resourceUri is absent.
///
/// # Errors
/// Malformed nested metadata, visibility, or resource URIs are explicit errors.
pub fn tool_meta(meta: Option<&Value>) -> Result<UiToolMeta, AppsError> {
    let Some(meta) = meta else {
        return Ok(UiToolMeta::default());
    };
    let meta = object(meta)?;
    let mut result = if let Some(ui) = meta.get("ui") {
        object(ui)?;
        reject_nulls(ui, &["resourceUri", "visibility"])?;
        decode(ui)?
    } else {
        UiToolMeta::default()
    };
    if result.resource_uri.is_none() {
        if let Some(uri) = meta.get("ui/resourceUri") {
            result.resource_uri = Some(
                uri.as_str()
                    .ok_or_else(|| AppsError::Metadata("ui/resourceUri must be a string".into()))?
                    .to_owned(),
            );
        }
    }
    if let Some(uri) = &result.resource_uri {
        validate_uri(uri)?;
    }
    Ok(result)
}

/// Decode resource `_meta.ui` while retaining unknown fields.
///
/// # Errors
/// Invalid security field shapes, nulls, or non-origin CSP sources are rejected.
/// Hosts remain responsible for CSP policy and host-specific domain validation.
pub fn resource_meta(meta: Option<&Value>) -> Result<UiResourceMeta, AppsError> {
    let meta = meta.map(object).transpose()?;
    let Some(ui) = meta.and_then(|meta| meta.get("ui")) else {
        return Ok(UiResourceMeta::default());
    };
    object(ui)?;
    reject_nulls(ui, &["csp", "permissions", "domain", "prefersBorder"])?;
    if let Some(csp) = ui.get("csp") {
        object(csp)?;
        reject_nulls(
            csp,
            &[
                "connectDomains",
                "resourceDomains",
                "frameDomains",
                "baseUriDomains",
            ],
        )?;
    }
    if let Some(permissions) = ui.get("permissions") {
        object(permissions)?;
        reject_nulls(
            permissions,
            &["camera", "microphone", "geolocation", "clipboardWrite"],
        )?;
    }
    let result: UiResourceMeta = decode(ui)?;
    if let Some(csp) = &result.csp {
        for source in csp
            .connect_domains
            .iter()
            .chain(&csp.resource_domains)
            .chain(&csp.frame_domains)
            .chain(&csp.base_uri_domains)
            .flatten()
        {
            validate_csp_origin(source)?;
        }
    }
    Ok(result)
}

fn validate_csp_origin(source: &str) -> Result<(), AppsError> {
    let invalid = || AppsError::Metadata(format!("invalid CSP origin: {source}"));
    if source.chars().any(|c| c.is_whitespace() || c.is_control())
        || source.contains([';', '\'', '"', '\\'])
    {
        return Err(invalid());
    }
    // Replace only the spec-supported leading wildcard subdomain for URL parsing.
    let normalized = source.replacen("://*.", "://wildcard.", 1);
    let uri = url::Url::parse(&normalized).map_err(|_| invalid())?;
    let authority = normalized
        .strip_prefix(&format!("{}://", uri.scheme()))
        .ok_or_else(invalid)?;
    if authority.trim_end_matches('/').contains('/') || authority.ends_with("//") {
        return Err(invalid());
    }
    if !matches!(uri.scheme(), "http" | "https" | "ws" | "wss")
        || uri.host_str().is_none()
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.query().is_some()
        || uri.fragment().is_some()
        || uri.path() != "/"
        || normalized.contains('*')
    {
        return Err(invalid());
    }
    Ok(())
}

/// Get the optional Apps capability from the core client capabilities object.
///
/// # Errors
/// An advertised extension without a string-array mimeTypes field is invalid.
pub fn ui_capability(capabilities: &Value) -> Result<Option<UiCapability>, AppsError> {
    object(capabilities)?;
    let Some(extensions) = capabilities.get("extensions") else {
        return Ok(None);
    };
    let Some(ui) = object(extensions)?.get(UI_EXTENSION) else {
        return Ok(None);
    };
    object(ui)?;
    decode(ui).map(Some)
}

/// Decoded UI content. Metadata remains available in both raw and typed forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiResource {
    /// Exact resource URI returned by the server.
    pub uri: String,
    /// UTF-8 HTML document, not sanitized or trusted.
    pub html: String,
    /// Validated Apps metadata.
    pub ui: UiResourceMeta,
    /// Complete resource-content metadata, including unrelated vendor fields.
    pub meta: Option<Map<String, Value>>,
}

/// Decode one resources/read result, enforcing exact URI/MIME and byte bounds.
/// HTML validation is deliberately limited to a doctype/html document envelope;
/// it is not a sanitizer or a full HTML parser. Hosts must sandbox the document.
///
/// # Errors
/// Rejects multiple entries, ambiguous text/blob, malformed metadata, invalid
/// base64/UTF-8, missing document envelope, or a decoded size above max_bytes.
pub fn decode_resource(
    result: &crate::McpResourceResult,
    expected_uri: &str,
    max_bytes: usize,
) -> Result<UiResource, AppsError> {
    validate_uri(expected_uri)?;
    let contents = result
        .raw
        .get("contents")
        .and_then(Value::as_array)
        .ok_or(AppsError::Content)?;
    if contents.len() != 1 {
        return Err(AppsError::Content);
    }
    let content = contents[0].as_object().ok_or(AppsError::Content)?;
    if content.get("uri").and_then(Value::as_str) != Some(expected_uri) {
        return Err(AppsError::Uri);
    }
    if content.get("mimeType").and_then(Value::as_str) != Some(UI_MIME_TYPE) {
        return Err(AppsError::MimeType);
    }
    let html = match (content.get("text"), content.get("blob")) {
        (Some(Value::String(text)), None) => {
            if text.len() > max_bytes {
                return Err(AppsError::TooLarge);
            }
            text.clone()
        }
        (None, Some(Value::String(blob))) => {
            // Bound the decoding allocation before parsing untrusted base64.
            let encoded_limit = max_bytes
                .saturating_add(2)
                .saturating_div(3)
                .saturating_mul(4);
            if blob.len() > encoded_limit {
                return Err(AppsError::TooLarge);
            }
            let bytes = STANDARD.decode(blob).map_err(|_| AppsError::Base64)?;
            if bytes.len() > max_bytes {
                return Err(AppsError::TooLarge);
            }
            String::from_utf8(bytes).map_err(|_| AppsError::Utf8)?
        }
        _ => return Err(AppsError::Content),
    };
    let envelope = html
        .trim_start_matches('\u{feff}')
        .trim()
        .to_ascii_lowercase();
    if !envelope.starts_with("<!doctype html>")
        || !envelope.contains("<html")
        || !envelope.contains("</html>")
    {
        return Err(AppsError::HtmlDocument);
    }
    let meta = content
        .get("_meta")
        .map(|value| object(value).cloned())
        .transpose()?;
    let ui = resource_meta(content.get("_meta"))?;
    Ok(UiResource {
        uri: expected_uri.to_owned(),
        html,
        ui,
        meta,
    })
}

/// Implementation identity in the iframe handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiImplementation {
    /// Application or host name.
    pub name: String,
    /// Application or host version.
    pub version: String,
    /// Additional implementation metadata.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// UI display modes supported by this revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiDisplayMode {
    /// Embedded in host flow.
    Inline,
    /// Fullscreen view.
    Fullscreen,
    /// Picture in picture.
    Pip,
}

/// MCP-style tools or resources capability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiListCapability {
    /// Supports list-change notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
    /// Additional capability settings.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// View capabilities in ui/initialize.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiAppCapabilities {
    /// Experimental capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<UiFeature>,
    /// Tools provided by the view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<UiListCapability>,
    /// Supported display modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_display_modes: Option<Vec<UiDisplayMode>>,
    /// Unknown capabilities.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Sandbox configuration applied by the host (not merely requested).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSandboxCapabilities {
    /// Granted browser permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<UiPermissions>,
    /// Approved CSP origins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csp: Option<UiCsp>,
    /// Unknown sandbox settings.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Host capabilities returned in ui/initialize.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiHostCapabilities {
    /// Experimental features.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<UiFeature>,
    /// Supports external link opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_links: Option<UiFeature>,
    /// Can proxy tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tools: Option<UiListCapability>,
    /// Can proxy resource reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_resources: Option<UiListCapability>,
    /// Accepts logging messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<UiFeature>,
    /// Applied sandbox configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<UiSandboxCapabilities>,
    /// Unknown host capabilities.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Host context. Stable common fields are typed; host-specific styling, sizing,
/// toolInfo and device fields are preserved in extra for forward compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiHostContext {
    /// Host color theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<UiTheme>,
    /// Current display mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_mode: Option<UiDisplayMode>,
    /// Supported host display modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_display_modes: Option<Vec<String>>,
    /// BCP 47 language preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// IANA time zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    /// Host identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Additional host context fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Host theme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiTheme {
    /// Light theme.
    Light,
    /// Dark theme.
    Dark,
}

/// Params for ui/initialize, separate from the core MCP protocol handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiInitializeParams {
    /// Must negotiate UI_PROTOCOL_VERSION, not the core MCP revision.
    pub protocol_version: String,
    /// View identity.
    pub app_info: UiImplementation,
    /// View capabilities.
    pub app_capabilities: UiAppCapabilities,
    /// Unknown request fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Result of ui/initialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiInitializeResult {
    /// Negotiated iframe protocol version.
    pub protocol_version: String,
    /// Host identity.
    pub host_info: UiImplementation,
    /// Host capabilities.
    pub host_capabilities: UiHostCapabilities,
    /// Initial host context, if supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_context: Option<UiHostContext>,
    /// Unknown result fields, including _meta.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Params for complete or partial tool-input notifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiToolInputParams {
    /// Complete or best-effort partial arguments object.
    pub arguments: Map<String, Value>,
    /// Unknown notification fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Tool-result notification params, retaining raw content blocks and _meta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiToolResultParams {
    /// Standard MCP content blocks, including future content types.
    pub content: Vec<Value>,
    /// UI-oriented structured data.
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    /// Tool execution error marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Raw result metadata, not model-facing.
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
    /// Unknown result fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl TryFrom<&crate::McpToolResult> for UiToolResultParams {
    type Error = AppsError;

    fn try_from(result: &crate::McpToolResult) -> Result<Self, Self::Error> {
        decode(&result.raw)
    }
}

/// Params for ui/resource-teardown; the host may omit its reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiResourceTeardownParams {
    /// Optional teardown reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unknown request fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
