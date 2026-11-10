//! Server-owned pending interactions. Observation never owns the underlying work.
#![allow(missing_docs)]

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use waltr::grpc::waltr::session::v1 as wire;
use crate::{ClientEffect, ClientError, ClientState, ClientUpdate};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InteractionIdentity {
    pub session_id: String,
    pub generation: String,
    pub id: String,
    pub operation_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InteractionRequestView {
    Permission { message: String, identity: String, claims: Vec<String>, suggestions: Vec<String> },
    Question { questions: Vec<InteractionQuestionView> },
    Form { message: String, fields: Vec<InteractionFormField> },
    UrlConsent { message: String, url: String },
    Unsupported { message: String, form: bool },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InteractionQuestionView {
    pub question: String,
    pub header: String,
    pub options: Vec<InteractionOptionView>,
    pub multiple: bool,
    pub custom: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct InteractionOptionView { pub label: String, pub description: String }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InteractionFormField {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub default: Option<InteractionFormValue>,
    pub choice_titles: BTreeMap<String, String>,
    pub schema: InteractionFieldSchema,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InteractionFieldSchema {
    String { choices: Option<Vec<String>>, #[serde(rename = "minLength")] min_length: Option<u64>, #[serde(rename = "maxLength")] max_length: Option<u64>, format: Option<String> },
    Number { integer: bool, minimum: Option<f64>, maximum: Option<f64> },
    Boolean,
    StringArray { choices: Vec<String>, #[serde(rename = "minItems")] min_items: Option<u64>, #[serde(rename = "maxItems")] max_items: Option<u64> },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(untagged)]
pub enum InteractionFormValue { String(String), Number(f64), Boolean(bool), Strings(Vec<String>) }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InteractionResponse {
    Permission { decision: InteractionPermissionDecision, #[serde(rename = "suggestionIndices")] suggestion_indices: Vec<usize>, message: Option<String> },
    Question { answers: Vec<Vec<String>> },
    Form { action: InteractionElicitationAction, values: BTreeMap<String, InteractionFormValue> },
    UrlConsent { action: InteractionElicitationAction },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum InteractionPermissionDecision { AllowOnce, AllowAlways, Deny, CancelWork }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum InteractionElicitationAction { Accept, Decline, Cancel }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PendingInteractionView {
    pub identity: InteractionIdentity,
    pub request: InteractionRequestView,
    pub responding: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionInteractionsView {
    pub generation: Option<String>,
    pub revision: u64,
    pub connected: bool,
    pub error: Option<String>,
    pub pending: Vec<PendingInteractionView>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InteractionClientState {
    pub sessions: BTreeMap<String, SessionInteractionsView>,
    pub(crate) watches: BTreeMap<String, InteractionWatch>,
    next_epoch: u64,
}
impl InteractionClientState {
    pub(crate) fn invalidate(&mut self, retain: bool) {
        self.sessions.clear();
        if !retain { self.watches.clear(); }
        for watch in self.watches.values_mut() {
            self.next_epoch += 1;
            let interests = watch.interests;
            *watch = InteractionWatch { interests, epoch: self.next_epoch, ..Default::default() };
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct InteractionWatch {
    pub interests: usize,
    pub epoch: u64,
    pub opening: bool,
    pub active: bool,
    pub retrying: bool,
    pub wire: Option<wire::InteractionSnapshot>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum InteractionEvent {
    Retain { session_id: String },
    Release { session_id: String },
    Opened { session_id: String, epoch: u64 },
    Snapshot { session_id: String, epoch: u64, snapshot: wire::InteractionSnapshot },
    Refreshed { session_id: String, epoch: u64, baseline: Option<(String, u64)>, snapshot: wire::InteractionSnapshot },
    RefreshFailed { session_id: String, epoch: u64, baseline: Option<(String, u64)>, error: ClientError },
    Failed { session_id: String, epoch: u64, error: ClientError },
    Retry { session_id: String, epoch: u64 },
    Respond { identity: InteractionIdentity, response: InteractionResponse },
    Responded { identity: InteractionIdentity, epoch: u64, error: Option<ClientError> },
}
#[derive(Clone, Debug, PartialEq)]
pub enum InteractionEffect {
    Watch { session_id: String, epoch: u64 },
    Sleep { session_id: String, epoch: u64 },
    Respond { identity: InteractionIdentity, epoch: u64, request: wire::RespondInteractionRequest },
    Refresh { session_id: String, epoch: u64, baseline: Option<(String, u64)> },
}
impl Eq for InteractionEffect {}

pub(crate) fn maintain(state: &mut ClientState) -> ClientUpdate {
    if !state.permits_watch_maintenance() { return ClientUpdate::default(); }
    let mut update = ClientUpdate::default();
    for (session_id, watch) in &mut state.interactions.watches {
        if watch.interests > 0 && !watch.opening && !watch.active && !watch.retrying {
            watch.opening = true;
            update.extend(effect(InteractionEffect::Watch { session_id: session_id.clone(), epoch: watch.epoch }));
        }
    }
    update
}
fn effect(effect: InteractionEffect) -> ClientUpdate { ClientUpdate::with_effect(ClientEffect::Interaction(effect)) }

pub(crate) fn reduce(state: &mut ClientState, event: InteractionEvent) -> ClientUpdate {
    match event {
        InteractionEvent::RefreshFailed { session_id, epoch, baseline, error } => {
            let current = state.interactions.watches.get(&session_id).and_then(|w| w.wire.as_ref()).map(|s| (s.generation.clone(), s.revision));
            if current != baseline { return ClientUpdate::default(); }
            reduce(state, InteractionEvent::Failed { session_id, epoch, error })
        }
        InteractionEvent::Refreshed { session_id, epoch, baseline, snapshot } => {
            let current = state.interactions.watches.get(&session_id).and_then(|w| w.wire.as_ref()).map(|s| (s.generation.clone(), s.revision));
            if current != baseline { return ClientUpdate::default(); }
            reduce(state, InteractionEvent::Snapshot { session_id, epoch, snapshot })
        }
        InteractionEvent::Retain { session_id } => {
            state.interactions.next_epoch += 1;
            let next_epoch = state.interactions.next_epoch;
            let watch = state.interactions.watches.entry(session_id.clone()).or_default();
            if watch.interests == 0 { watch.epoch = next_epoch; }
            watch.interests += 1;
            state.interactions.sessions.entry(session_id).or_default();
            maintain(state)
        }
        InteractionEvent::Release { session_id } => {
            if let Some(watch) = state.interactions.watches.get_mut(&session_id) {
                watch.interests = watch.interests.saturating_sub(1);
                if watch.interests == 0 {
                    state.interactions.watches.remove(&session_id);
                    state.interactions.sessions.remove(&session_id);
                }
            }
            ClientUpdate::default()
        }
        InteractionEvent::Opened { session_id, epoch } => {
            if let Some(watch) = current_watch(state, &session_id, epoch) { watch.opening = false; watch.active = true; }
            ClientUpdate::default()
        }
        InteractionEvent::Snapshot { session_id, epoch, snapshot } => {
            let Some(watch) = current_watch(state, &session_id, epoch) else { return ClientUpdate::default(); };
            if watch.wire.as_ref().is_some_and(|old| old.generation == snapshot.generation && old.revision > snapshot.revision) {
                return ClientUpdate::default();
            }
            let old = state.interactions.sessions.get(&session_id).cloned().unwrap_or_default();
            let pending = snapshot.pending.iter().filter(|p| p.session_id == session_id).map(|p| {
                let identity = InteractionIdentity { session_id: p.session_id.clone(), generation: snapshot.generation.clone(), id: p.id.clone(), operation_id: p.operation_id.clone() };
                let previous = old.pending.iter().find(|p| p.identity == identity);
                PendingInteractionView { identity, request: request_view(p), responding: previous.is_some_and(|p| p.responding), error: previous.and_then(|p| p.error.clone()) }
            }).collect();
            state.interactions.sessions.insert(session_id.clone(), SessionInteractionsView { generation: Some(snapshot.generation.clone()), revision: snapshot.revision, connected: true, error: None, pending });
            state.interactions.watches.get_mut(&session_id).unwrap().wire = Some(snapshot);
            ClientUpdate::default()
        }
        InteractionEvent::Failed { session_id, epoch, error } => {
            if current_watch(state, &session_id, epoch).is_none() { return ClientUpdate::default(); }
            state.interactions.next_epoch += 1;
            let epoch = state.interactions.next_epoch;
            let watch = state.interactions.watches.get_mut(&session_id).unwrap();
            watch.epoch = epoch;
            watch.active = false; watch.opening = false; watch.retrying = true;
            if let Some(view) = state.interactions.sessions.get_mut(&session_id) {
                view.connected = false; view.error = Some(error.to_string());
                for pending in &mut view.pending {
                    if pending.responding { pending.error = Some("Connection lost; response outcome is not yet known".into()); }
                    pending.responding = false;
                }
            }
            effect(InteractionEffect::Sleep { session_id, epoch })
        }
        InteractionEvent::Retry { session_id, epoch } => {
            if let Some(watch) = current_watch(state, &session_id, epoch) { watch.retrying = false; }
            maintain(state)
        }
        InteractionEvent::Respond { identity, response } => {
            if !state.permits_watch_maintenance() { return ClientUpdate::default(); }
            let Some(watch) = state.interactions.watches.get(&identity.session_id) else { return ClientUpdate::default(); };
            let epoch = watch.epoch;
            let Some(view) = state.interactions.sessions.get_mut(&identity.session_id) else { return ClientUpdate::default(); };
            let connected = view.connected;
            let Some(pending) = view.pending.iter_mut().find(|p| p.identity == identity) else { return ClientUpdate::default(); };
            if pending.responding { return ClientUpdate::default(); }
            if !connected { pending.error = Some("Reconnect before responding".into()); return ClientUpdate::default(); }
            match response_wire(&identity, &pending.request, watch.wire.as_ref(), response) {
                Ok(request) => { pending.responding = true; pending.error = None; effect(InteractionEffect::Respond { identity, epoch, request }) }
                Err(error) => { pending.error = Some(error.to_string()); ClientUpdate::default() }
            }
        }
        InteractionEvent::Responded { identity, epoch, error } => {
            if current_watch(state, &identity.session_id, epoch).is_none() { return ClientUpdate::default(); }
            let Some(pending) = state.interactions.sessions.get_mut(&identity.session_id).and_then(|v| v.pending.iter_mut().find(|p| p.identity == identity)) else { return ClientUpdate::default(); };
            pending.responding = error.is_none();
            pending.error = error.map(|e| e.to_string());
            // Even a successful response is not an authoritative local removal.
            let baseline = state.interactions.watches.get(&identity.session_id).and_then(|w| w.wire.as_ref()).map(|s| (s.generation.clone(), s.revision));
            effect(InteractionEffect::Refresh { session_id: identity.session_id, epoch, baseline })
        }
    }
}
fn current_watch<'a>(state: &'a mut ClientState, session: &str, epoch: u64) -> Option<&'a mut InteractionWatch> {
    if !state.permits_protected_requests() { return None; }
    state.interactions.watches.get_mut(session).filter(|w| w.interests > 0 && w.epoch == epoch)
}
pub(crate) fn event_is_current(state: &ClientState, event: &InteractionEvent) -> bool {
    if let InteractionEvent::Responded { identity, .. } = event {
        if !state.interactions.sessions.get(&identity.session_id).is_some_and(|s| s.pending.iter().any(|p| p.identity == *identity && p.responding)) { return false; }
    }
    if let InteractionEvent::Refreshed { session_id, baseline, .. } | InteractionEvent::RefreshFailed { session_id, baseline, .. } = event {
        if state.interactions.watches.get(session_id).and_then(|w| w.wire.as_ref()).map(|s| (s.generation.clone(), s.revision)) != *baseline { return false; }
    }
    let (session_id, epoch) = match event {
        InteractionEvent::Opened { session_id, epoch }
        | InteractionEvent::Snapshot { session_id, epoch, .. }
        | InteractionEvent::Refreshed { session_id, epoch, .. }
        | InteractionEvent::RefreshFailed { session_id, epoch, .. }
        | InteractionEvent::Failed { session_id, epoch, .. }
        | InteractionEvent::Retry { session_id, epoch } => (session_id, epoch),
        InteractionEvent::Responded { identity, epoch, .. } => (&identity.session_id, epoch),
        _ => return true,
    };
    state.permits_protected_requests() && state.interactions.watches.get(session_id).is_some_and(|w| w.interests > 0 && w.epoch == *epoch)
}
pub(crate) fn effect_is_current(state: &ClientState, effect: &InteractionEffect) -> bool {
    let (session_id, epoch) = match effect {
        InteractionEffect::Watch { session_id, epoch }
        | InteractionEffect::Sleep { session_id, epoch }
        | InteractionEffect::Refresh { session_id, epoch, .. } => (session_id, epoch),
        InteractionEffect::Respond { identity, epoch, .. } => (&identity.session_id, epoch),
    };
    if !state.permits_protected_requests() || !state.interactions.watches.get(session_id).is_some_and(|w| w.interests > 0 && w.epoch == *epoch) { return false; }
    match effect {
        InteractionEffect::Respond { identity, .. } => state.interactions.sessions.get(session_id).is_some_and(|s| s.pending.iter().any(|p| p.identity == *identity && p.responding)),
        InteractionEffect::Refresh { baseline, .. } => state.interactions.watches.get(session_id).and_then(|w| w.wire.as_ref()).map(|s| (s.generation.clone(), s.revision)) == *baseline,
        _ => true,
    }
}

fn request_view(pending: &wire::PendingInteraction) -> InteractionRequestView {
    use wire::pending_interaction::Request as R;
    match pending.request.as_ref() {
        Some(R::Permission(p)) => {
            let parse = |json: &str| serde_json::from_str::<Vec<serde_json::Value>>(json).map(|v| v.into_iter().map(|v| v.to_string()).collect());
            match (parse(&p.claims_json), parse(&p.suggested_patterns_json)) {
                (Ok(claims), Ok(suggestions)) => InteractionRequestView::Permission { message: p.message.clone(), identity: p.identity.clone(), claims, suggestions },
                _ => InteractionRequestView::Unsupported { message: "Invalid permission descriptor".into(), form: false },
            }
        }
        Some(R::Question(q)) => InteractionRequestView::Question { questions: q.questions.iter().map(|q| InteractionQuestionView { question: q.question.clone(), header: q.header.clone(), options: q.options.iter().map(|o| InteractionOptionView { label: o.label.clone(), description: o.description.clone() }).collect(), multiple: q.multiple, custom: q.custom }).collect() },
        Some(R::Form(f)) => match parse_form(&f.requested_schema_json) {
            Ok(fields) => InteractionRequestView::Form { message: f.message.clone(), fields },
            Err(error) => InteractionRequestView::Unsupported { message: format!("{}: {error}", f.message), form: true },
        },
        Some(R::UrlConsent(u)) => InteractionRequestView::UrlConsent { message: u.message.clone(), url: u.url.clone() },
        None => InteractionRequestView::Unsupported { message: "Missing interaction descriptor".into(), form: false },
    }
}

fn invalid() -> ClientError { ClientError::new("Invalid interaction response") }
fn unsupported() -> ClientError { ClientError::new("Unsupported interaction form schema") }

fn response_wire(identity: &InteractionIdentity, view: &InteractionRequestView, snapshot: Option<&wire::InteractionSnapshot>, response: InteractionResponse) -> Result<wire::RespondInteractionRequest, ClientError> {
    use wire::respond_interaction_request::Response as R;
    let response = match (view, response) {
        (InteractionRequestView::Permission { suggestions, .. }, InteractionResponse::Permission { decision, suggestion_indices, message }) => {
            if decision != InteractionPermissionDecision::AllowAlways && !suggestion_indices.is_empty() { return Err(invalid()); }
            let pending = snapshot.and_then(|s| s.pending.iter().find(|p| p.id == identity.id && p.operation_id == identity.operation_id)).ok_or_else(invalid)?;
            let Some(wire::pending_interaction::Request::Permission(p)) = pending.request.as_ref() else { return Err(invalid()); };
            let patterns: Vec<serde_json::Value> = serde_json::from_str(&p.suggested_patterns_json).map_err(|_| invalid())?;
            let mut selected = Vec::new();
            for i in suggestion_indices { if i >= suggestions.len() { return Err(invalid()); } selected.push(patterns.get(i).ok_or_else(invalid)?.clone()); }
            R::Permission(wire::PermissionInteractionResponse { decision: match decision { InteractionPermissionDecision::AllowOnce => 1, InteractionPermissionDecision::AllowAlways => 2, InteractionPermissionDecision::Deny => 3, InteractionPermissionDecision::CancelWork => 4 }, patterns_json: serde_json::to_string(&selected).map_err(|_| invalid())?, message })
        }
        (InteractionRequestView::Question { questions }, InteractionResponse::Question { answers }) => {
            if questions.len() != answers.len() { return Err(invalid()); }
            for (q, a) in questions.iter().zip(&answers) {
                if a.is_empty() || (!q.multiple && a.len() != 1) || a.iter().any(|v| v.is_empty() || (!q.custom && !q.options.iter().any(|o| o.label == *v))) { return Err(invalid()); }
                if a.iter().enumerate().any(|(i, v)| a[..i].contains(v)) { return Err(invalid()); }
            }
            R::Question(wire::QuestionInteractionResponse { answers: answers.into_iter().map(|values| wire::InteractionQuestionAnswer { values }).collect() })
        }
        (InteractionRequestView::Form { fields, .. }, InteractionResponse::Form { action, values }) => {
            if action == InteractionElicitationAction::Accept { validate_form(fields, &values)?; }
            else if !values.is_empty() { return Err(invalid()); }
            R::Form(wire::ElicitationInteractionResponse { action: action_wire(action), content_json: if action == InteractionElicitationAction::Accept { Some(serde_json::to_string(&values).map_err(|_| invalid())?) } else { None } })
        }
        (InteractionRequestView::UrlConsent { .. }, InteractionResponse::UrlConsent { action }) => R::UrlConsent(wire::ElicitationInteractionResponse { action: action_wire(action), content_json: None }),
        // An unsupported form can be declined/cancelled, never implicitly accepted.
        (InteractionRequestView::Unsupported { form: true, .. }, InteractionResponse::Form { action, values }) if action != InteractionElicitationAction::Accept && values.is_empty() => R::Form(wire::ElicitationInteractionResponse { action: action_wire(action), content_json: None }),
        _ => return Err(invalid()),
    };
    Ok(wire::RespondInteractionRequest { session_id: identity.session_id.clone(), generation: identity.generation.clone(), id: identity.id.clone(), operation_id: identity.operation_id.clone(), response: Some(response) })
}
fn action_wire(action: InteractionElicitationAction) -> i32 { match action { InteractionElicitationAction::Accept => 1, InteractionElicitationAction::Decline => 2, InteractionElicitationAction::Cancel => 3 } }

fn parse_form(json: &str) -> Result<Vec<InteractionFormField>, ClientError> {
    use serde_json::Value;
    let schema: Value = serde_json::from_str(json).map_err(|_| unsupported())?;
    let root = schema.as_object().ok_or_else(unsupported)?;
    if root.get("type").and_then(Value::as_str) != Some("object") || root.keys().any(|k| !["type", "properties", "required", "additionalProperties", "title", "description", "$schema"].contains(&k.as_str())) || root.get("additionalProperties").is_some_and(|v| v != &Value::Bool(false)) { return Err(unsupported()); }
    let properties = root.get("properties").and_then(Value::as_object).ok_or_else(unsupported)?;
    let required = match root.get("required") { None => Vec::new(), Some(v) => v.as_array().ok_or_else(unsupported)?.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(unsupported)).collect::<Result<Vec<_>, _>>()? };
    if required.iter().any(|key| !properties.contains_key(key)) { return Err(unsupported()); }
    let fields: Vec<InteractionFormField> = properties.iter().map(|(name, value)| {
        let p = value.as_object().ok_or_else(unsupported)?;
        let ty = p.get("type").and_then(Value::as_str).ok_or_else(unsupported)?;
        let allowed: &[&str] = match ty {
            "string" => &["type", "title", "description", "default", "enum", "enumNames", "oneOf", "minLength", "maxLength", "format"],
            "number" | "integer" => &["type", "title", "description", "default", "minimum", "maximum"],
            "boolean" => &["type", "title", "description", "default"],
            "array" => &["type", "title", "description", "default", "items", "minItems", "maxItems"],
            _ => return Err(unsupported()),
        };
        if p.keys().any(|k| !allowed.contains(&k.as_str())) { return Err(unsupported()); }
        let text = |key: &str| p.get(key).map(|v| v.as_str().map(str::to_owned).ok_or_else(unsupported)).transpose();
        let uint = |key: &str| p.get(key).map(|v| v.as_u64().ok_or_else(unsupported)).transpose();
        let number = |key: &str| p.get(key).map(|v| v.as_f64().ok_or_else(unsupported)).transpose();
        for (lower, upper) in [("minLength", "maxLength"), ("minItems", "maxItems"), ("minimum", "maximum")] {
            if number(lower)?.zip(number(upper)?).is_some_and(|(l, u)| l > u) { return Err(unsupported()); }
        }
        let mut choice_titles = BTreeMap::new();
        let schema = match ty {
            "string" => {
                let format = text("format")?;
                if format.as_deref().is_some_and(|f| !["email", "uri", "date", "date-time"].contains(&f)) { return Err(unsupported()); }
                let choices = form_choices(p, "oneOf", &mut choice_titles)?;
                InteractionFieldSchema::String { choices, min_length: uint("minLength")?, max_length: uint("maxLength")?, format }
            }
            "number" | "integer" => InteractionFieldSchema::Number { integer: ty == "integer", minimum: number("minimum")?, maximum: number("maximum")? },
            "boolean" => InteractionFieldSchema::Boolean,
            "array" => {
                let items = p.get("items").and_then(Value::as_object).ok_or_else(unsupported)?;
                if !(items.len() == 2 && items.get("type").and_then(Value::as_str) == Some("string") && items.contains_key("enum")) && !(items.len() == 1 && items.contains_key("anyOf")) { return Err(unsupported()); }
                InteractionFieldSchema::StringArray { choices: form_choices(items, "anyOf", &mut choice_titles)?.ok_or_else(unsupported)?, min_items: uint("minItems")?, max_items: uint("maxItems")? }
            }
            _ => return Err(unsupported()),
        };
        let default = p.get("default").map(|v| serde_json::from_value(v.clone()).map_err(|_| unsupported())).transpose()?;
        Ok(InteractionFormField { name: name.clone(), title: text("title")?, description: text("description")?, required: required.contains(name), default, choice_titles, schema })
    }).collect::<Result<_, _>>()?;
    for field in &fields {
        if let Some(default) = &field.default { validate_form(std::slice::from_ref(field), &BTreeMap::from([(field.name.clone(), default.clone())])).map_err(|_| unsupported())?; }
    }
    Ok(fields)
}

fn form_choices(schema: &serde_json::Map<String, serde_json::Value>, titled: &str, titles: &mut BTreeMap<String, String>) -> Result<Option<Vec<String>>, ClientError> {
    let strings = |v: &serde_json::Value| -> Result<Vec<String>, ClientError> {
        let values = v.as_array().ok_or_else(unsupported)?;
        if values.is_empty() { return Err(unsupported()); }
        values.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(unsupported)).collect()
    };
    let mut choices = schema.get("enum").map(strings).transpose()?;
    if let Some(names) = schema.get("enumNames") {
        let names = strings(names)?;
        let values = choices.as_ref().ok_or_else(unsupported)?;
        if names.len() != values.len() { return Err(unsupported()); }
        titles.extend(values.iter().cloned().zip(names));
    }
    if let Some(alternatives) = schema.get(titled) {
        let alternatives = alternatives.as_array().filter(|a| !a.is_empty()).ok_or_else(unsupported)?;
        let mut values = Vec::new();
        for alternative in alternatives {
            let object = alternative.as_object().filter(|o| o.len() == 2).ok_or_else(unsupported)?;
            let value = object.get("const").and_then(serde_json::Value::as_str).ok_or_else(unsupported)?.to_owned();
            let title = object.get("title").and_then(serde_json::Value::as_str).ok_or_else(unsupported)?.to_owned();
            titles.insert(value.clone(), title);
            values.push(value);
        }
        if let Some(choices) = &mut choices { choices.retain(|v| values.contains(v)); }
        else { choices = Some(values); }
    }
    Ok(choices)
}

fn validate_form(fields: &[InteractionFormField], values: &BTreeMap<String, InteractionFormValue>) -> Result<(), ClientError> {
    if values.keys().any(|key| !fields.iter().any(|f| &f.name == key)) { return Err(invalid()); }
    for field in fields {
        let Some(value) = values.get(&field.name) else { if field.required { return Err(invalid()); } continue; };
        let valid = match (&field.schema, value) {
            (InteractionFieldSchema::String { choices, min_length, max_length, format }, InteractionFormValue::String(v)) => choices.as_ref().is_none_or(|c| c.contains(v)) && min_length.is_none_or(|n| v.chars().count() as u64 >= n) && max_length.is_none_or(|n| v.chars().count() as u64 <= n) && format.as_deref().is_none_or(|format| valid_string_format(v, format)),
            (InteractionFieldSchema::Number { integer, minimum, maximum }, InteractionFormValue::Number(v)) => v.is_finite() && (!integer || v.fract() == 0.0) && minimum.is_none_or(|n| *v >= n) && maximum.is_none_or(|n| *v <= n),
            (InteractionFieldSchema::Boolean, InteractionFormValue::Boolean(_)) => true,
            (InteractionFieldSchema::StringArray { choices, min_items, max_items }, InteractionFormValue::Strings(v)) => v.iter().all(|s| choices.contains(s)) && min_items.is_none_or(|n| v.len() as u64 >= n) && max_items.is_none_or(|n| v.len() as u64 <= n),
            _ => false,
        };
        if !valid { return Err(invalid()); }
    }
    Ok(())
}

fn valid_string_format(value: &str, format: &str) -> bool {
    match format {
        "email" => {
            if value.chars().any(|c| matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')) { return false; }
            let Some((local, domain)) = value.split_once('@') else { return false; };
            !local.is_empty() && !domain.contains('@') && domain.match_indices('.').any(|(index, _)| index > 0 && index + 1 < domain.len())
        }
        "uri" => url::Url::parse(value).is_ok(),
        "date" => valid_calendar_date(value.as_bytes()),
        "date-time" => valid_date_time(value.as_bytes()),
        _ => false,
    }
}

fn decimal_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) { return None; }
    Some(bytes.iter().fold(0, |number, digit| number * 10 + u32::from(digit - b'0')))
}

fn valid_calendar_date(bytes: &[u8]) -> bool {
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' { return false; }
    let (Some(year), Some(month), Some(day)) = (decimal_digits(&bytes[..4]), decimal_digits(&bytes[5..7]), decimal_digits(&bytes[8..])) else { return false; };
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn valid_date_time(bytes: &[u8]) -> bool {
    if bytes.len() < 20 || !valid_calendar_date(&bytes[..10]) || !matches!(bytes[10], b'T' | b't') || bytes[13] != b':' || bytes[16] != b':' { return false; }
    if !matches!(decimal_digits(&bytes[11..13]), Some(0..=23)) || !matches!(decimal_digits(&bytes[14..16]), Some(0..=59)) || !matches!(decimal_digits(&bytes[17..19]), Some(0..=59)) { return false; }
    let mut offset = 19;
    if bytes[offset] == b'.' {
        offset += 1;
        let start = offset;
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) { offset += 1; }
        if offset == start { return false; }
    }
    let zone = &bytes[offset..];
    matches!(zone, [b'Z' | b'z']) || (zone.len() == 6 && matches!(zone[0], b'+' | b'-') && zone[3] == b':' && matches!(decimal_digits(&zone[1..3]), Some(0..=23)) && matches!(decimal_digits(&zone[4..]), Some(0..=59)))
}

#[cfg(test)]
mod tests;
