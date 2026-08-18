//! Self-contained provider config section: state, input, and rendering.
//!
//! This module owns the provider list selection, the create/edit form, and the
//! delete confirmation. It is hosted by the config surface shell behind the
//! "Providers" tab and does not depend on the surrounding tab chrome.

use std::collections::BTreeMap;

use waltr_client::{
    AuthStatusSnapshot, ClientAuthTarget, ClientCommand, ClientOperationView, ClientSnapshot,
    ConfigView, ModelCapabilitiesView, ModelCatalogMatchView, ModelLimitsView, ModelOptionsView,
    ModelTypeView, ModelView, OAuthJwtMetadataView, OAuthProviderView, OAuthTokenRequestStyleView,
    OAuthUrlParamView, ProviderKindView, ProviderView, ResourceEditBaselineView,
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    screens::config::auth::{ConfigAuthState, status_can_authorize, status_can_reset, status_for},
    screens::{OperationResult, OperationTracker, SectionAction},
    theme::THEME,
    ui,
};

/// Provider form mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderFormMode {
    /// Create a new provider.
    Create,
    /// Edit the selected provider.
    Edit,
}

/// Active provider form state.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderForm {
    /// Form mode.
    pub mode: ProviderFormMode,
    /// Active field index.
    pub field: usize,
    /// Original provider name for edits.
    pub original_name: Option<String>,
    original_provider: Option<ProviderView>,
    baseline: Option<ResourceEditBaselineView>,
    /// Provider name draft.
    pub name: String,
    /// Provider kind draft.
    pub kind: ProviderKindView,
    /// Provider URL draft.
    pub url: String,
    oauth_compatible: bool,
    oauth_settings: Option<OAuthProviderView>,
    oauth_scopes: String,
    oauth_auth_params: String,
    oauth_jwt_metadata: String,
    oauth_auth_params_error: Option<String>,
    oauth_jwt_metadata_error: Option<String>,
    headers: BTreeMap<String, String>,
    models: BTreeMap<String, ModelView>,
    selected_model: usize,
    model_form: Option<ModelForm>,
    delete_model: Option<String>,
}

const PROVIDER_BASE_FIELD_COUNT: usize = 4;
const PROVIDER_OAUTH_FIELD_COUNT: usize = 10;

impl ProviderForm {
    const fn create() -> Self {
        Self {
            mode: ProviderFormMode::Create,
            field: 0,
            original_name: None,
            original_provider: None,
            baseline: None,
            name: String::new(),
            kind: ProviderKindView::AnthropicMessages,
            url: String::new(),
            oauth_compatible: false,
            oauth_settings: None,
            oauth_scopes: String::new(),
            oauth_auth_params: String::new(),
            oauth_jwt_metadata: String::new(),
            oauth_auth_params_error: None,
            oauth_jwt_metadata_error: None,
            headers: BTreeMap::new(),
            models: BTreeMap::new(),
            selected_model: 0,
            model_form: None,
            delete_model: None,
        }
    }

    fn edit(
        (_index, provider): (usize, &ProviderView),
        baseline: ResourceEditBaselineView,
    ) -> Self {
        let oauth_settings = provider.oauth_settings.clone();
        Self {
            mode: ProviderFormMode::Edit,
            field: 0,
            original_name: Some(provider.name.clone()),
            original_provider: Some(provider.clone()),
            baseline: Some(baseline),
            name: provider.name.clone(),
            kind: provider.kind,
            url: provider.url.clone(),
            oauth_compatible: provider.oauth_compatible,
            oauth_scopes: oauth_settings
                .as_ref()
                .map_or_else(String::new, |settings| settings.scopes.join(", ")),
            oauth_auth_params: oauth_settings
                .as_ref()
                .map_or_else(String::new, |settings| {
                    format_auth_params(&settings.auth_url_params)
                }),
            oauth_jwt_metadata: oauth_settings
                .as_ref()
                .map_or_else(String::new, |settings| {
                    format_jwt_metadata(&settings.jwt_metadata)
                }),
            oauth_auth_params_error: None,
            oauth_jwt_metadata_error: None,
            oauth_settings,
            headers: provider.headers.clone(),
            models: provider.models.clone(),
            selected_model: 0,
            model_form: None,
            delete_model: None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        match key.code {
            KeyCode::Tab | KeyCode::Down => self.move_focus_next(),
            KeyCode::BackTab => {
                self.move_focus_previous();
            }
            KeyCode::Up => self.move_focus_previous(),
            KeyCode::Right | KeyCode::Enter if self.focused_model_index().is_some() => {
                self.open_model_edit_form();
            }
            KeyCode::Backspace | KeyCode::Delete if self.focused_model_index().is_some() => {
                self.arm_model_delete();
            }
            KeyCode::Delete if self.field == 3 => {
                self.oauth_compatible = false;
                self.oauth_settings = None;
                self.oauth_scopes.clear();
                self.oauth_auth_params.clear();
                self.oauth_jwt_metadata.clear();
            }
            KeyCode::Backspace if self.is_text_field() => {
                let _ = self.active_text_mut().pop();
                self.clear_structured_error();
            }
            KeyCode::Left | KeyCode::Right if self.field == 1 => self.toggle_kind(),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.field == 3 => {
                self.oauth_compatible = !self.oauth_compatible;
                if self.oauth_compatible && self.oauth_settings.is_none() {
                    self.oauth_settings = Some(empty_oauth_settings());
                }
            }
            KeyCode::Left | KeyCode::Right if self.field == 10 => self.toggle_request_style(),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.field == 11 => {
                if let Some(settings) = &mut self.oauth_settings {
                    settings.send_state_in_token_request = !settings.send_state_in_token_request;
                }
            }
            KeyCode::Enter => return FormAction::Submit,
            KeyCode::Char(character) if self.is_text_field() => {
                self.active_text_mut().push(character);
                self.clear_structured_error();
            }
            _ => {}
        }

        FormAction::Continue
    }

    fn move_focus_next(&mut self) {
        self.field = (self.field + 1) % self.focus_count();
        self.sync_selected_model_to_focus();
    }

    fn move_focus_previous(&mut self) {
        let focus_count = self.focus_count();
        self.field = (self.field + focus_count - 1) % focus_count;
        self.sync_selected_model_to_focus();
    }

    fn focus_count(&self) -> usize {
        self.model_field_offset() + self.models.len()
    }

    fn focused_model_index(&self) -> Option<usize> {
        self.field
            .checked_sub(self.model_field_offset())
            .filter(|index| *index < self.models.len())
    }

    fn model_is_focused(&self) -> bool {
        self.focused_model_index().is_some()
    }

    fn sync_selected_model_to_focus(&mut self) {
        if let Some(index) = self.focused_model_index() {
            self.selected_model = index.min(self.models.len().saturating_sub(1));
        }
    }

    fn provider(mut self) -> ProviderView {
        if let Some(settings) = &mut self.oauth_settings {
            settings.scopes = split_list(&self.oauth_scopes);
            if let Ok(params) = parse_auth_params(&self.oauth_auth_params) {
                settings.auth_url_params = params;
            }
            if let Ok(metadata) = parse_jwt_metadata(&self.oauth_jwt_metadata) {
                settings.jwt_metadata = metadata;
            }
        }
        ProviderView {
            kind: self.kind,
            name: self.name,
            url: self.url,
            headers: self.headers,
            oauth_compatible: self.oauth_compatible,
            oauth_settings: self.oauth_settings,
            models: self.models,
        }
    }

    const fn model_form(&self) -> Option<&ModelForm> {
        self.model_form.as_ref()
    }

    fn open_model_create_form(&mut self) {
        self.delete_model = None;
        self.model_form = Some(ModelForm::create());
    }

    fn open_model_edit_form(&mut self) {
        let Some((key, model)) = self.selected_model_view() else {
            return;
        };
        let form = ModelForm::edit(key, model);
        self.delete_model = None;
        self.model_form = Some(form);
    }

    fn arm_model_delete(&mut self) {
        self.model_form = None;
        self.delete_model = self.selected_model_view().map(|(key, _)| key.clone());
    }

    fn submit_model_form(&mut self) -> FormAction {
        let Some(form) = &self.model_form else {
            return FormAction::Continue;
        };

        if !form.has_type() {
            return FormAction::Continue;
        }

        let new_key = form.key.clone();
        let original_key = form.original_key.clone();
        let model = form.model();
        if let Some(original_key) = original_key
            && original_key != new_key
        {
            self.models.remove(&original_key);
        }
        self.models.insert(new_key, model);
        self.selected_model = self.selected_model.min(self.models.len().saturating_sub(1));
        self.field = self.model_field_offset() + self.selected_model;
        self.model_form = None;
        FormAction::Continue
    }

    fn delete_selected_model(&mut self, model_key: &str) {
        self.delete_model = None;
        self.models.remove(model_key);
        self.selected_model = self.selected_model.min(self.models.len().saturating_sub(1));
        self.field = if self.models.is_empty() {
            self.model_field_offset() - 1
        } else {
            self.model_field_offset() + self.selected_model
        };
    }

    fn selected_model_view(&self) -> Option<(&String, &ModelView)> {
        self.models.iter().nth(self.selected_model)
    }

    const fn active_text_mut(&mut self) -> &mut String {
        match self.field {
            2 => &mut self.url,
            4 => &mut self.oauth_settings.as_mut().expect("OAuth field").name,
            5 => {
                &mut self
                    .oauth_settings
                    .as_mut()
                    .expect("OAuth field")
                    .authorize_url
            }
            6 => &mut self.oauth_settings.as_mut().expect("OAuth field").token_url,
            7 => &mut self.oauth_settings.as_mut().expect("OAuth field").client_id,
            8 => {
                &mut self
                    .oauth_settings
                    .as_mut()
                    .expect("OAuth field")
                    .redirect_uri
            }
            9 => &mut self.oauth_scopes,
            12 => &mut self.oauth_auth_params,
            13 => &mut self.oauth_jwt_metadata,
            _ => &mut self.name,
        }
    }

    const fn is_text_field(&self) -> bool {
        matches!(self.field, 0 | 2)
            || (self.oauth_settings.is_some()
                && matches!(self.field, 4 | 5 | 6 | 7 | 8 | 9 | 12 | 13))
    }

    fn clear_structured_error(&mut self) {
        match self.field {
            12 => self.oauth_auth_params_error = None,
            13 => self.oauth_jwt_metadata_error = None,
            _ => {}
        }
    }

    fn validate_structured_fields(&mut self) -> bool {
        if self.oauth_settings.is_none() {
            return true;
        }
        self.oauth_auth_params_error = parse_auth_params(&self.oauth_auth_params)
            .err()
            .map(|()| "authorization parameters must be a valid JSON array".to_owned());
        self.oauth_jwt_metadata_error = parse_jwt_metadata(&self.oauth_jwt_metadata)
            .err()
            .map(|()| "JWT metadata mappings must be a valid JSON array".to_owned());
        self.oauth_auth_params_error.is_none() && self.oauth_jwt_metadata_error.is_none()
    }

    const fn model_field_offset(&self) -> usize {
        if self.oauth_settings.is_some() {
            PROVIDER_BASE_FIELD_COUNT + PROVIDER_OAUTH_FIELD_COUNT
        } else {
            PROVIDER_BASE_FIELD_COUNT
        }
    }

    const fn toggle_request_style(&mut self) {
        if let Some(settings) = &mut self.oauth_settings {
            settings.token_request_style = match settings.token_request_style {
                OAuthTokenRequestStyleView::Unspecified
                | OAuthTokenRequestStyleView::Unknown
                | OAuthTokenRequestStyleView::FormUrlencoded => OAuthTokenRequestStyleView::Json,
                OAuthTokenRequestStyleView::Json => OAuthTokenRequestStyleView::FormUrlencoded,
            };
        }
    }

    const fn toggle_kind(&mut self) {
        self.kind = match self.kind {
            ProviderKindView::AnthropicMessages => ProviderKindView::OpenAiChatCompletions,
            ProviderKindView::OpenAiChatCompletions => ProviderKindView::OpenAiResponses,
            ProviderKindView::OpenAiResponses => ProviderKindView::OpenAiResponsesWebsocket,
            _ => ProviderKindView::AnthropicMessages,
        };
    }
}

enum FormAction {
    Continue,
    Submit,
}

const fn empty_oauth_settings() -> OAuthProviderView {
    OAuthProviderView {
        name: String::new(),
        authorize_url: String::new(),
        token_url: String::new(),
        client_id: String::new(),
        redirect_uri: String::new(),
        scopes: Vec::new(),
        token_request_style: OAuthTokenRequestStyleView::FormUrlencoded,
        auth_url_params: Vec::new(),
        jwt_metadata: Vec::new(),
        send_state_in_token_request: false,
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn format_auth_params(params: &[OAuthUrlParamView]) -> String {
    serde_json::to_string(params).unwrap_or_else(|_| "[]".to_owned())
}

fn parse_auth_params(value: &str) -> Result<Vec<OAuthUrlParamView>, ()> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(value.trim()).map_err(|_| ())
}

fn format_jwt_metadata(metadata: &[OAuthJwtMetadataView]) -> String {
    serde_json::to_string(metadata).unwrap_or_else(|_| "[]".to_owned())
}

fn parse_jwt_metadata(value: &str) -> Result<Vec<OAuthJwtMetadataView>, ()> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(value.trim()).map_err(|_| ())
}

/// Model form mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFormMode {
    /// Create a new model.
    Create,
    /// Edit the selected model.
    Edit,
}

/// Active model form state.
///
/// Models are a keyed collection inside a provider. Saving rides inside an
/// [`ClientCommand::UpdateProvider`] carrying the whole provider; there are no
/// model-level commands.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelForm {
    /// Form mode.
    pub mode: ModelFormMode,
    /// Active field index.
    pub field: usize,
    /// Original model key for edits.
    pub original_key: Option<String>,
    /// Model key draft.
    pub key: String,
    /// Model display name draft.
    pub name: String,
    /// Model type draft. `None` until the user picks one.
    pub kind: Option<ModelTypeView>,
    /// Sampling temperature draft.
    pub temperature: String,
    /// Supported reasoning effort levels draft.
    pub reasoning_efforts: String,
    /// Maximum context window draft.
    pub context: String,
    /// Maximum output token draft.
    pub output: String,
    /// Tool-calling capability draft.
    pub tools: bool,
    /// Image-input capability draft.
    pub image: bool,
    /// Catalog linkage preserved while editing model fields.
    pub catalog_match: Option<ModelCatalogMatchView>,
    /// Pricing preserved while editing model fields.
    pub cost: Option<waltr_client::ModelCostView>,
}

const MODEL_FIELD_COUNT: usize = 8;

impl ModelForm {
    const fn create() -> Self {
        Self {
            mode: ModelFormMode::Create,
            field: 0,
            original_key: None,
            key: String::new(),
            name: String::new(),
            kind: None,
            temperature: String::new(),
            reasoning_efforts: String::new(),
            context: String::new(),
            output: String::new(),
            tools: false,
            image: false,
            catalog_match: None,
            cost: None,
        }
    }

    fn edit(key: &str, model: &ModelView) -> Self {
        Self {
            mode: ModelFormMode::Edit,
            field: 0,
            original_key: Some(key.to_owned()),
            key: key.to_owned(),
            name: model.name.clone(),
            kind: Some(model.kind),
            temperature: model
                .options
                .temperature
                .map_or_else(String::new, |value| value.to_string()),
            reasoning_efforts: model.options.reasoning_efforts.join(", "),
            context: model
                .limits
                .context
                .map_or_else(String::new, |value| value.to_string()),
            output: model
                .limits
                .output
                .map_or_else(String::new, |value| value.to_string()),
            tools: model.capabilities.tools,
            image: model.capabilities.image,
            catalog_match: model.catalog_match.clone(),
            cost: model.cost.clone(),
        }
    }

    /// Whether the form has a model type selected. Saving is blocked while this
    /// is false.
    #[must_use]
    pub const fn has_type(&self) -> bool {
        self.kind.is_some()
    }

    fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        match key.code {
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % MODEL_FIELD_COUNT,
            KeyCode::BackTab | KeyCode::Up => {
                self.field = (self.field + MODEL_FIELD_COUNT - 1) % MODEL_FIELD_COUNT;
            }
            KeyCode::Left | KeyCode::Right if self.field == 2 => self.cycle_kind(),
            KeyCode::Left | KeyCode::Right if self.field == 6 => self.tools = !self.tools,
            KeyCode::Left | KeyCode::Right if self.field == 7 => self.image = !self.image,
            KeyCode::Backspace if self.is_text_field() => drop(self.active_text_mut().pop()),
            KeyCode::Enter => return FormAction::Submit,
            KeyCode::Char(character) if self.is_text_field() => {
                self.active_text_mut().push(character);
            }
            _ => {}
        }

        FormAction::Continue
    }

    const fn is_text_field(&self) -> bool {
        matches!(self.field, 0 | 1 | 3 | 4 | 5)
    }

    const fn active_text_mut(&mut self) -> &mut String {
        match self.field {
            1 => &mut self.name,
            3 => &mut self.temperature,
            4 => &mut self.context,
            5 => &mut self.reasoning_efforts,
            _ => &mut self.key,
        }
    }

    const fn cycle_kind(&mut self) {
        self.kind = Some(match self.kind {
            None
            | Some(ModelTypeView::Tts | ModelTypeView::Unspecified | ModelTypeView::Unknown) => {
                ModelTypeView::Llm
            }
            Some(ModelTypeView::Llm) => ModelTypeView::Embedding,
            Some(ModelTypeView::Embedding) => ModelTypeView::Stt,
            Some(ModelTypeView::Stt) => ModelTypeView::Tts,
        });
    }

    fn model(&self) -> ModelView {
        ModelView {
            name: self.name.clone(),
            kind: self.kind.unwrap_or(ModelTypeView::Unspecified),
            options: ModelOptionsView {
                temperature: self.temperature.trim().parse().ok(),
                top_p: None,
                top_k: None,
                min_p: None,
                presence_penalty: None,
                repetition_penalty: None,
                reasoning_efforts: split_reasoning_efforts(&self.reasoning_efforts),
            },
            limits: ModelLimitsView {
                context: self.context.trim().parse().ok(),
                output: self.output.trim().parse().ok(),
            },
            capabilities: ModelCapabilitiesView {
                tools: self.tools,
                image: self.image,
            },
            catalog_match: self.catalog_match.clone(),
            cost: self.cost.clone(),
        }
    }
}

/// Self-contained provider config section state and behavior.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderSection {
    config_form: Option<ProviderForm>,
    delete_provider: Option<String>,
    selected_provider: usize,
    patch: OperationTracker,
    resource_baselines: BTreeMap<String, ResourceEditBaselineView>,
}

impl ProviderSection {
    /// Create an empty provider section.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config_form: None,
            delete_provider: None,
            selected_provider: 0,
            patch: OperationTracker::new(),
            resource_baselines: BTreeMap::new(),
        }
    }

    /// Return the selected provider index.
    #[must_use]
    pub const fn selected_provider(&self) -> usize {
        self.selected_provider
    }

    /// Return the active provider form, if any.
    #[must_use]
    pub const fn config_form(&self) -> Option<&ProviderForm> {
        self.config_form.as_ref()
    }

    /// Return the provider pending delete confirmation, if any.
    #[must_use]
    pub fn delete_provider(&self) -> Option<&str> {
        self.delete_provider.as_deref()
    }

    /// Whether a transient overlay (form or delete prompt) is active.
    #[must_use]
    pub fn has_overlay(&self) -> bool {
        self.config_form
            .as_ref()
            .is_some_and(|form| form.model_form.is_some() || form.delete_model.is_some())
            || self.config_form.is_some()
            || self.delete_provider.is_some()
    }

    /// Reconcile a submitted patch against the shared reducer operation state.
    pub fn reconcile_patch(&mut self, snapshot: &ClientSnapshot) {
        self.resource_baselines = snapshot.config.resource_baselines.clone();
        if let Some(result) = self
            .patch
            .reconcile(&snapshot.config.patch, snapshot, |operation| {
                let ClientOperationView::ConfigPatch { status, error, .. } = operation else {
                    return None;
                };

                Some(match *status {
                    "ready" => OperationResult::Ready(()),
                    "failed" => OperationResult::Failed(
                        error.as_deref().unwrap_or("config patch failed").to_owned(),
                    ),
                    _ => OperationResult::Pending,
                })
            })
            && result.is_ok()
        {
            self.config_form = None;
            self.delete_provider = None;
        }
    }

    /// Whether the model sub-editor is open for some provider.
    #[must_use]
    pub fn in_model_editor(&self) -> bool {
        self.model_form().is_some()
    }

    /// Return the active model form, if any.
    #[must_use]
    pub fn model_form(&self) -> Option<&ModelForm> {
        self.config_form.as_ref().and_then(ProviderForm::model_form)
    }

    /// Clamp the selection to the available providers after a snapshot update.
    pub const fn sync_selection(&mut self, provider_count: usize) {
        if provider_count == 0 {
            self.selected_provider = 0;
        } else if self.selected_provider >= provider_count {
            self.selected_provider = provider_count - 1;
        }
    }

    /// Dismiss any active overlay. Returns true if something was dismissed.
    ///
    /// Provider-owned model overlays dismiss before the provider form itself.
    pub fn dismiss_overlay(&mut self) -> bool {
        if self.patch.is_pending() {
            return false;
        }

        if let Some(form) = &mut self.config_form
            && (form.model_form.take().is_some() || form.delete_model.take().is_some())
        {
            return true;
        }

        let dismissed = self.config_form.take().is_some() || self.delete_provider.take().is_some();
        if dismissed {
            self.patch.clear_error();
        }
        dismissed
    }

    /// Open the selected provider form so its model list can be edited.
    pub fn open_model_editor(&mut self, config: Option<&ConfigView>) {
        if self.patch.is_pending() {
            return;
        }

        self.delete_provider = None;
        self.patch.clear_error();
        self.config_form = self.selected_provider_view(config).and_then(|selected| {
            let path = format!("providers/{}.yaml", selected.1.name);
            self.resource_baselines
                .get(&path)
                .cloned()
                .map(|baseline| ProviderForm::edit(selected, baseline))
        });
    }

    /// Open a create form. When a provider form is open this creates a model;
    /// otherwise it creates a provider.
    pub fn open_create_form(&mut self) {
        if self.patch.is_pending() {
            return;
        }

        if let Some(form) = &mut self.config_form {
            form.open_model_create_form();
            return;
        }

        self.delete_provider = None;
        self.patch.clear_error();
        self.config_form = Some(ProviderForm::create());
    }

    /// Arm the delete confirmation for the current selection. When a provider
    /// editor is open this targets the selected model; otherwise the provider.
    pub fn arm_delete(&mut self, config: Option<&ConfigView>) {
        if self.patch.is_pending() {
            return;
        }

        if let Some(form) = &mut self.config_form {
            if form.model_is_focused() {
                form.arm_model_delete();
            }
            return;
        }

        self.config_form = None;
        self.patch.clear_error();
        self.delete_provider = self.selected_provider_name(config);
    }

    /// Route a key event into the section, using the current config snapshot.
    #[allow(
        clippy::too_many_lines,
        reason = "ordered provider form, confirmation, and credential routing stays auditable"
    )]
    pub fn handle_key(&mut self, key: KeyEvent, config: Option<&ConfigView>) -> SectionAction {
        if self.patch.is_pending() {
            return SectionAction::Continue;
        }

        if key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return SectionAction::Unhandled;
        }

        if let Some(form) = &mut self.config_form {
            if let Some(model_form) = &mut form.model_form {
                if matches!(model_form.handle_key(key), FormAction::Submit) {
                    form.submit_model_form();
                }
                return SectionAction::Continue;
            }

            if let Some(key_name) = form.delete_model.clone() {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        form.delete_selected_model(&key_name);
                        SectionAction::Continue
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        form.delete_model = None;
                        SectionAction::Continue
                    }
                    _ => SectionAction::Continue,
                };
            }

            return match form.handle_key(key) {
                FormAction::Continue => SectionAction::Continue,
                FormAction::Submit => self.submit_form(),
            };
        }

        if let Some(name) = self.delete_provider.clone() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.delete_selected(&name),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.delete_provider = None;
                    SectionAction::Continue
                }
                _ => SectionAction::Continue,
            };
        }

        match key.code {
            KeyCode::Up => {
                self.selected_provider = self.selected_provider.saturating_sub(1);
                SectionAction::Continue
            }
            KeyCode::Down => {
                let max = provider_count(config).saturating_sub(1);
                self.selected_provider = (self.selected_provider + 1).min(max);
                SectionAction::Continue
            }
            KeyCode::Right | KeyCode::Enter => {
                self.open_model_editor(config);
                SectionAction::Continue
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.arm_delete(config);
                SectionAction::Continue
            }
            _ => SectionAction::Unhandled,
        }
    }

    fn submit_form(&mut self) -> SectionAction {
        let Some(form) = &mut self.config_form else {
            return SectionAction::Continue;
        };
        if !form.validate_structured_fields() {
            return SectionAction::Continue;
        }

        let mode = form.mode;
        let original_name = form.original_name.clone();
        let original = form.original_provider.clone();
        let baseline = form.baseline.clone();
        let provider = form.clone().provider();
        self.patch.begin();
        match mode {
            ProviderFormMode::Create => {
                SectionAction::ClientCommand(ClientCommand::AddProvider { provider })
            }
            ProviderFormMode::Edit => {
                let Some(original_name) = original_name else {
                    return SectionAction::Continue;
                };
                let (Some(original), Some(baseline)) = (original, baseline) else {
                    return SectionAction::Continue;
                };
                SectionAction::ClientCommand(ClientCommand::UpdateProvider {
                    original_name,
                    original: Box::new(original),
                    baseline,
                    provider,
                })
            }
        }
    }

    fn delete_selected(&mut self, name: &str) -> SectionAction {
        self.patch.begin();
        SectionAction::ClientCommand(ClientCommand::DeleteProvider {
            name: name.to_owned(),
        })
    }

    fn selected_provider_view<'a>(
        &self,
        config: Option<&'a ConfigView>,
    ) -> Option<(usize, &'a ProviderView)> {
        config?
            .providers
            .values()
            .nth(self.selected_provider)
            .map(|provider| (self.selected_provider, provider))
    }

    fn selected_provider_name(&self, config: Option<&ConfigView>) -> Option<String> {
        self.selected_provider_view(config)
            .map(|(_, provider)| provider.name.clone())
    }

    pub(crate) fn selected_auth_target(
        &self,
        config: Option<&ConfigView>,
    ) -> Option<ClientAuthTarget> {
        self.selected_provider_name(config).map(provider_target)
    }

    /// Render the section content (provider list) into `area`.
    ///
    /// `loaded` indicates the watch stream has delivered config; when false the
    /// section shows a waiting placeholder.
    #[allow(
        clippy::too_many_arguments,
        reason = "rendering consumes the complete shared config and auth snapshot state"
    )]
    pub(crate) fn render_content(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        config: Option<&ConfigView>,
        loaded: bool,
        statuses: &BTreeMap<String, AuthStatusSnapshot>,
        auth: &ConfigAuthState,
    ) {
        let lines = if loaded {
            config.map_or_else(loading_lines, |config| {
                config_lines(
                    config,
                    self.selected_provider,
                    self.delete_provider(),
                    self.patch.is_pending(),
                    self.patch.error(),
                    statuses,
                    auth,
                )
            })
        } else {
            loading_lines()
        };

        frame.render_widget(
            Paragraph::new(lines)
                .style(THEME.panel_style())
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    /// Render the create/edit form overlay, if active.
    pub fn render_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(form) = self.model_form() {
            let form_area = ui::centered_percent_rect(area, 82, 16);
            frame.render_widget(Clear, area);
            frame.render_widget(Clear, form_area);
            render_model_form_widget(frame, form_area, form);
            return;
        }

        if let Some(form) = &self.config_form {
            let (width, height) = if form.oauth_settings.is_some() {
                (92, 22)
            } else {
                (82, 15)
            };
            let form_area = ui::centered_percent_rect(area, width, height);
            frame.render_widget(Clear, area);
            frame.render_widget(Clear, form_area);
            render_form_widget(
                frame,
                form_area,
                form,
                self.patch.is_pending(),
                self.patch.error(),
            );
        }
    }
}

fn provider_count(config: Option<&ConfigView>) -> usize {
    config.map_or(0, |config| config.providers.len())
}

fn loading_lines() -> Vec<Line<'static>> {
    vec![ui::loading_line("Waiting for configuration...")]
}

#[allow(
    clippy::too_many_arguments,
    reason = "line rendering receives explicit config, patch, and credential presentation state"
)]
fn config_lines(
    config: &ConfigView,
    selected_provider: usize,
    delete_provider: Option<&str>,
    pending_patch: bool,
    patch_error: Option<&str>,
    statuses: &BTreeMap<String, AuthStatusSnapshot>,
    auth: &ConfigAuthState,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if config.providers.is_empty() {
        lines.push(ui::loading_line("No providers configured."));
        return lines;
    }

    for (index, provider) in config.providers.values().enumerate() {
        let selected = index == selected_provider;
        let target = provider_target(provider.name.clone());
        let auth_status = status_for(statuses, &target);
        let deleting = delete_provider == Some(provider.name.as_str());
        let resetting = auth.reset_matches(&target);
        let auth_pending = auth.pending_for(&target);
        lines.push(provider_line(
            provider,
            selected,
            deleting,
            pending_patch,
            patch_error,
            auth_status,
            resetting,
            auth_pending,
        ));
        if selected && !deleting && !resetting {
            lines.push(auth_status_line(auth_status));
            lines.push(auth_action_line(auth_status, auth_pending));
            lines.extend(auth_status_detail_lines(auth_status, false));
        }
    }

    let selected_target = config
        .providers
        .values()
        .nth(selected_provider)
        .map(|provider| provider_target(provider.name.clone()));
    if let Some(error) = selected_target
        .as_ref()
        .and_then(|target| auth.error_for(target))
    {
        lines.push(ui::error_line(error));
    }

    lines
}

#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "one provider row directly renders orthogonal selection and operation states"
)]
fn provider_line(
    provider: &ProviderView,
    selected: bool,
    deleting: bool,
    pending_patch: bool,
    patch_error: Option<&str>,
    auth_status: Option<&AuthStatusSnapshot>,
    resetting: bool,
    auth_pending: bool,
) -> Line<'static> {
    if deleting {
        return Line::from(vec![
            ui::selection_marker(selected),
            Span::raw(" "),
            ui::selected_title(provider.name.clone()),
            Span::raw("  "),
            ui::delete_prompt_span(ui::delete_prompt_text(pending_patch, patch_error)),
        ]);
    }

    if resetting {
        return Line::from(vec![
            ui::selection_marker(selected),
            Span::raw(" "),
            ui::selected_title(provider.name.clone()),
            Span::raw("  "),
            ui::delete_prompt_span(if auth_pending {
                "Resetting credential..."
            } else {
                "Reset credential? Enter/y confirm, n/Esc cancel"
            }),
        ]);
    }

    let mut spans = vec![
        ui::selection_marker(selected),
        Span::raw(" "),
        ui::selected_title(provider.name.clone()),
        Span::raw("  "),
        Span::styled(provider_kind_name(provider.kind), THEME.focus_style()),
        Span::raw("  "),
        Span::styled(
            oauth_compatible_name(provider.oauth_compatible),
            THEME.text_style(),
        ),
        Span::raw("  "),
        Span::styled(provider_url(provider), THEME.text_style()),
    ];
    if !selected {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            auth_status_label(auth_status),
            auth_status_style(auth_status),
        ));
    }
    Line::from(spans)
}

fn provider_url(provider: &ProviderView) -> String {
    if provider.url.is_empty() {
        "no-url".to_owned()
    } else {
        provider.url.clone()
    }
}

fn provider_target(key: String) -> ClientAuthTarget {
    ClientAuthTarget {
        kind: "provider".to_owned(),
        key,
    }
}

pub(super) fn auth_status_label(status: Option<&AuthStatusSnapshot>) -> String {
    let Some(status) = status else {
        return "status unavailable".to_owned();
    };
    match status.kind.as_str() {
        "oauthConfigured" => "connected (OAuth)",
        "apiKeyConfigured" => "connected (API key)",
        "expired" => "expired",
        "authorizationRequired" => "authorization required",
        "unsupported" => "unsupported",
        "notConfigured" => "not connected",
        _ => "unknown",
    }
    .to_owned()
}

pub(super) fn auth_status_style(status: Option<&AuthStatusSnapshot>) -> Style {
    match status.map(|status| status.kind.as_str()) {
        Some("oauthConfigured" | "apiKeyConfigured") => THEME.focus_style(),
        Some("expired" | "authorizationRequired") => THEME.danger_style(),
        _ => THEME.muted_text_style(),
    }
}

pub(super) fn auth_status_detail_lines(
    status: Option<&AuthStatusSnapshot>,
    mcp: bool,
) -> Vec<Line<'static>> {
    let Some(status) = status else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let message = safe_status_text(&status.message);
    if !message.is_empty() {
        for (index, chunk) in wrap_status_message(&message, 48).into_iter().enumerate() {
            lines.push(Line::from(vec![
                Span::raw(if index == 0 {
                    "    message: "
                } else {
                    "             "
                }),
                Span::styled(chunk, THEME.muted_text_style()),
            ]));
        }
    }
    if status.expires_at_ms > 0 {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!("expires: {}", format_expiration(status.expires_at_ms)),
                THEME.muted_text_style(),
            ),
        ]));
    }
    if mcp && let Some(method) = registration_method_label(&status.registration_method) {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("registration: {method}"), THEME.muted_text_style()),
        ]));
    }
    lines
}

pub(super) fn auth_status_line(status: Option<&AuthStatusSnapshot>) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
        Span::styled("status: ", THEME.muted_text_style()),
        Span::styled(auth_status_label(status), auth_status_style(status)),
    ])
}

fn safe_status_text(value: &str) -> String {
    const MAX_STATUS_MESSAGE_CHARS: usize = 120;
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim();
    if trimmed.chars().count() <= MAX_STATUS_MESSAGE_CHARS {
        return trimmed.to_owned();
    }
    let mut bounded = trimmed
        .chars()
        .take(MAX_STATUS_MESSAGE_CHARS - 3)
        .collect::<String>();
    bounded.push_str("...");
    bounded
}

fn wrap_status_message(value: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
        while current.chars().count() > width {
            let remainder = current.chars().skip(width).collect::<String>();
            lines.push(current.chars().take(width).collect());
            current = remainder;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn format_expiration(milliseconds: i64) -> String {
    let seconds = milliseconds.div_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

fn civil_date(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn registration_method_label(method: &str) -> Option<&'static str> {
    match method {
        "preregistered" => Some("Preregistered"),
        "clientMetadata" => Some("Client metadata"),
        "dynamic" => Some("Dynamic"),
        _ => None,
    }
}

pub(super) fn auth_action_hint(status: Option<&AuthStatusSnapshot>, pending: bool) -> String {
    if pending {
        return "auth action pending...".to_owned();
    }
    let Some(status) = status else {
        return "no credential actions".to_owned();
    };
    let authorize = if status_can_authorize(status) {
        if matches!(
            status.kind.as_str(),
            "oauthConfigured" | "expired" | "authorizationRequired"
        ) {
            "a reauthorize"
        } else {
            "a authorize"
        }
    } else {
        ""
    };
    let refresh = if status.can_refresh {
        "  f refresh"
    } else {
        ""
    };
    let reset = if status_can_reset(status) {
        "  r reset"
    } else {
        ""
    };
    let hint = format!("{authorize}{refresh}{reset}");
    if hint.is_empty() {
        "no credential actions".to_owned()
    } else {
        hint.trim_start().to_owned()
    }
}

pub(super) fn auth_action_line(
    status: Option<&AuthStatusSnapshot>,
    pending: bool,
) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("actions: {}", auth_action_hint(status, pending)),
            THEME.muted_text_style(),
        ),
    ])
}

fn render_form_widget(
    frame: &mut Frame<'_>,
    area: Rect,
    form: &ProviderForm,
    pending_patch: bool,
    patch_error: Option<&str>,
) {
    let title = match form.mode {
        ProviderFormMode::Create => "Create provider",
        ProviderFormMode::Edit => "Edit provider",
    };

    let body_area = super::render_modal_frame(
        frame,
        area,
        title,
        "Tab field Space toggle Del OAuth C-x c model Esc cancel",
    );
    let visible_height = frame
        .area()
        .bottom()
        .saturating_sub(body_area.y)
        .min(body_area.height);
    let structured_error_count = usize::from(form.oauth_auth_params_error.is_some())
        + usize::from(form.oauth_jwt_metadata_error.is_some());
    let mut focused_line = form.field
        + form
            .focused_model_index()
            .map_or(0, |_| 2 + structured_error_count);
    if form.field == 12 && form.oauth_auth_params_error.is_some() {
        focused_line = PROVIDER_BASE_FIELD_COUNT + PROVIDER_OAUTH_FIELD_COUNT;
    } else if form.field == 13 && form.oauth_jwt_metadata_error.is_some() {
        focused_line = PROVIDER_BASE_FIELD_COUNT
            + PROVIDER_OAUTH_FIELD_COUNT
            + usize::from(form.oauth_auth_params_error.is_some());
    }
    let scroll =
        u16::try_from(focused_line.saturating_sub(usize::from(visible_height.saturating_sub(1))))
            .unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(form_lines(
            form,
            pending_patch,
            patch_error,
            usize::from(body_area.width),
        ))
        .style(THEME.panel_style())
        .scroll((scroll, 0)),
        body_area,
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "provider fields and field-specific validation remain in terminal focus order"
)]
fn form_lines(
    form: &ProviderForm,
    pending_patch: bool,
    patch_error: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        form_line(0, form.field, "name", &form.name),
        form_line(1, form.field, "type", provider_kind_name(form.kind)),
        form_line(2, form.field, "url", &form.url),
        form_line(
            3,
            form.field,
            "oauth",
            oauth_compatible_name(form.oauth_compatible),
        ),
    ];

    if let Some(settings) = &form.oauth_settings {
        lines.extend([
            form_line(4, form.field, "oauth name", &settings.name),
            form_line(5, form.field, "authorize URL", &settings.authorize_url),
            form_line(6, form.field, "token URL", &settings.token_url),
            form_line(7, form.field, "client ID", &settings.client_id),
            form_line(8, form.field, "redirect URI", &settings.redirect_uri),
            form_line(9, form.field, "scopes", &form.oauth_scopes),
            form_line(
                10,
                form.field,
                "request style",
                token_request_style_name(settings.token_request_style),
            ),
            form_line(
                11,
                form.field,
                "send state",
                if settings.send_state_in_token_request {
                    "yes"
                } else {
                    "no"
                },
            ),
            structured_form_line(
                12,
                form.field,
                "auth params JSON",
                &form.oauth_auth_params,
                width,
            ),
            structured_form_line(
                13,
                form.field,
                "JWT mappings JSON",
                &form.oauth_jwt_metadata,
                width,
            ),
        ]);
    }

    for error in [
        form.oauth_auth_params_error.as_deref(),
        form.oauth_jwt_metadata_error.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            THEME.danger_style().add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Models staged in this provider draft",
        THEME.accent_bold(),
    )));

    if form.models.is_empty() {
        lines.push(ui::loading_line("No models configured."));
    } else {
        for (index, (key, model)) in form.models.iter().enumerate() {
            lines.push(model_line(
                key,
                model,
                form.focused_model_index() == Some(index),
                form.delete_model.as_deref() == Some(key.as_str()),
            ));
        }
    }

    if pending_patch {
        lines.push(ui::saving_line());
    } else if let Some(error) = patch_error {
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            THEME.danger_style().add_modifier(Modifier::BOLD),
        )));
    }

    lines
}

fn structured_form_line(
    index: usize,
    active: usize,
    label: &'static str,
    value: &str,
    width: usize,
) -> Line<'static> {
    let available = width.saturating_sub(label.chars().count() + 4);
    form_line(
        index,
        active,
        label,
        &truncate_with_ellipsis(value, available),
    )
}

fn truncate_with_ellipsis(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut truncated = value.chars().take(width - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}

const fn oauth_compatible_name(enabled: bool) -> &'static str {
    if enabled { "oauth" } else { "no-oauth" }
}

const fn token_request_style_name(style: OAuthTokenRequestStyleView) -> &'static str {
    match style {
        OAuthTokenRequestStyleView::Json => "json",
        OAuthTokenRequestStyleView::FormUrlencoded => "form-urlencoded",
        OAuthTokenRequestStyleView::Unspecified => "unspecified",
        OAuthTokenRequestStyleView::Unknown => "unknown",
    }
}

fn form_line(index: usize, active: usize, label: &'static str, value: &str) -> Line<'static> {
    ui::form_field_row(index, active, label, value)
}

const fn provider_kind_name(kind: ProviderKindView) -> &'static str {
    match kind {
        ProviderKindView::Unspecified => "unspecified",
        ProviderKindView::AnthropicMessages => "anthropic-messages",
        ProviderKindView::OpenAiChatCompletions => "openai-chat-completions",
        ProviderKindView::OpenAiResponses => "openai-responses",
        ProviderKindView::OpenAiResponsesWebsocket => "openai-responses-websocket",
        ProviderKindView::Unknown => "unknown",
    }
}

fn model_line(key: &str, model: &ModelView, selected: bool, deleting: bool) -> Line<'static> {
    let display = if model.name.is_empty() {
        key.to_owned()
    } else {
        model.name.clone()
    };

    if deleting {
        return Line::from(vec![
            ui::selection_marker(selected),
            Span::raw(" "),
            ui::selected_title(display),
            Span::raw("  "),
            ui::delete_prompt_span(ui::DELETE_CONFIRMATION_HINT),
        ]);
    }

    Line::from(vec![
        ui::selection_marker(selected),
        Span::raw(" "),
        ui::selected_title(display),
        Span::raw("  "),
        Span::styled(model_type_name(model.kind), THEME.focus_style()),
    ])
}

fn render_model_form_widget(frame: &mut Frame<'_>, area: Rect, form: &ModelForm) {
    let title = match form.mode {
        ModelFormMode::Create => "Create model",
        ModelFormMode::Edit => "Edit model",
    };

    let body_area = super::render_modal_frame(
        frame,
        area,
        title,
        "Tab field  Left/Right toggle  Enter save  Esc cancel",
    );
    frame.render_widget(
        Paragraph::new(model_form_lines(form))
            .style(THEME.panel_style())
            .wrap(Wrap { trim: false }),
        body_area,
    );
}

fn model_form_lines(form: &ModelForm) -> Vec<Line<'static>> {
    let mut lines = vec![
        form_line(0, form.field, "key", &form.key),
        form_line(1, form.field, "name", &form.name),
        model_type_line(2, form.field, form.kind),
        form_line(3, form.field, "temperature", &form.temperature),
        form_line(4, form.field, "context", &form.context),
        form_line(5, form.field, "reasoning", &form.reasoning_efforts),
        model_bool_line(6, form.field, "tools", form.tools),
        model_bool_line(7, form.field, "image", form.image),
    ];

    if let Some(catalog_match) = &form.catalog_match {
        lines.push(catalog_match_line(catalog_match));
    }

    if !form.has_type() {
        lines.push(Line::from(Span::styled(
            "type required before saving",
            THEME.danger_style().add_modifier(Modifier::BOLD),
        )));
    }

    lines
}

fn catalog_match_line(catalog_match: &ModelCatalogMatchView) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "catalog: {} / {} ({})",
            catalog_match.provider_id,
            catalog_match.model_id,
            catalog_match_source_name(catalog_match.source)
        ),
        THEME.muted_text_style(),
    ))
}

const fn catalog_match_source_name(source: waltr_client::CatalogMatchSourceView) -> &'static str {
    match source {
        waltr_client::CatalogMatchSourceView::Manual => "manual",
        waltr_client::CatalogMatchSourceView::Auto => "auto",
        waltr_client::CatalogMatchSourceView::Applied => "applied",
        waltr_client::CatalogMatchSourceView::Unspecified => "unspecified",
        waltr_client::CatalogMatchSourceView::Unknown => "unknown",
    }
}

fn split_reasoning_efforts(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn model_type_line(index: usize, active: usize, kind: Option<ModelTypeView>) -> Line<'static> {
    let (value, style) = kind.map_or_else(
        || {
            (
                "<unset>".to_owned(),
                THEME.danger_style().add_modifier(Modifier::BOLD),
            )
        },
        |kind| (model_type_name(kind).to_owned(), THEME.text_style()),
    );

    ui::form_field_row_styled(index, active, "type", value, style)
}

fn model_bool_line(index: usize, active: usize, label: &'static str, value: bool) -> Line<'static> {
    let text = if value { "yes" } else { "no" };
    ui::form_field_row(index, active, label, text)
}

const fn model_type_name(kind: ModelTypeView) -> &'static str {
    match kind {
        ModelTypeView::Unspecified => "unspecified",
        ModelTypeView::Llm => "llm",
        ModelTypeView::Embedding => "embedding",
        ModelTypeView::Stt => "stt",
        ModelTypeView::Tts => "tts",
        ModelTypeView::Unknown => "unknown",
    }
}
