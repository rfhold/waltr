use super::*;
use crate::{Client, ClientEvent, StatusEvent, SystemStatus};

fn ready() -> Client {
    let mut client = Client::new();
    client.apply(ClientEvent::Status(StatusEvent::WatchChanged(SystemStatus { ready: true })));
    client
}
fn apply(client: &mut Client, event: InteractionEvent) -> ClientUpdate { client.apply(ClientEvent::Interaction(event)) }
fn epoch(client: &Client) -> u64 { client.state().interactions.watches["session"].epoch }
fn pending(request: wire::pending_interaction::Request) -> wire::PendingInteraction {
    wire::PendingInteraction { id: "pending".into(), operation_id: "operation".into(), session_id: "session".into(), request: Some(request) }
}
fn question() -> wire::PendingInteraction {
    pending(wire::pending_interaction::Request::Question(wire::QuestionInteraction { questions: vec![wire::InteractionQuestion { question: "Continue?".into(), header: "Choice".into(), options: vec![wire::InteractionQuestionOption { label: "Yes".into(), description: "Proceed".into() }], multiple: false, custom: false }] }))
}
fn snapshot(generation: &str, revision: u64, pending: Vec<wire::PendingInteraction>) -> wire::InteractionSnapshot {
    wire::InteractionSnapshot { generation: generation.into(), revision, pending }
}
fn observe(client: &mut Client, snapshot: wire::InteractionSnapshot) {
    let epoch = epoch(client);
    apply(client, InteractionEvent::Snapshot { session_id: "session".into(), epoch, snapshot });
}
fn identity(client: &Client) -> InteractionIdentity { client.state().interactions.sessions["session"].pending[0].identity.clone() }
fn answer() -> InteractionResponse { InteractionResponse::Question { answers: vec![vec!["Yes".into()]] } }

#[test]
fn two_views_share_one_watch_and_exact_identity_until_authoritative_resolution() {
    let mut client = ready();
    assert!(matches!(client.retain_interactions("session").effects(), [ClientEffect::Interaction(InteractionEffect::Watch { .. })]));
    assert!(client.retain_interactions("session").is_empty());
    observe(&mut client, snapshot("generation", 1, vec![question()]));
    let first_view = client.state().interactions.sessions["session"].clone();
    let second_view = first_view.clone();
    assert_eq!(first_view.pending[0].identity, second_view.pending[0].identity);
    let identity = identity(&client);
    let update = client.respond_interaction(identity.clone(), answer());
    let [ClientEffect::Interaction(InteractionEffect::Respond { request, epoch, .. })] = update.effects() else { panic!("typed response"); };
    assert_eq!((&request.session_id, &request.generation, &request.id, &request.operation_id), (&identity.session_id, &identity.generation, &identity.id, &identity.operation_id));
    assert!(client.respond_interaction(identity.clone(), answer()).is_empty());
    let update = apply(&mut client, InteractionEvent::Responded { identity, epoch: *epoch, error: None });
    assert!(matches!(update.effects(), [ClientEffect::Interaction(InteractionEffect::Refresh { .. })]));
    assert_eq!(client.state().interactions.sessions["session"].pending.len(), 1);
    assert!(client.state().interactions.sessions["session"].pending[0].responding);
    observe(&mut client, snapshot("generation", 2, vec![]));
    assert!(client.state().interactions.sessions["session"].pending.is_empty());
}

#[test]
fn snapshots_replace_instead_of_merge_and_restart_replaces_identity() {
    let mut client = ready();
    client.retain_interactions("session");
    observe(&mut client, snapshot("first", 10, vec![question()]));
    let old = identity(&client);
    observe(&mut client, snapshot("first", 9, vec![]));
    assert_eq!(identity(&client), old);
    observe(&mut client, snapshot("second", 0, vec![question()]));
    assert_ne!(identity(&client), old);
    assert!(client.respond_interaction(old, answer()).is_empty());
    observe(&mut client, snapshot("second", 1, vec![]));
    assert!(client.state().interactions.sessions["session"].pending.is_empty());
}

#[test]
fn reconnect_fences_retired_snapshots_errors_and_completions_without_replaying() {
    let mut client = ready();
    client.retain_interactions("session");
    observe(&mut client, snapshot("first", 1, vec![question()]));
    let old_epoch = epoch(&client);
    let old_identity = identity(&client);
    let update = apply(&mut client, InteractionEvent::Failed { session_id: "session".into(), epoch: old_epoch, error: ClientError::new("offline") });
    assert!(matches!(update.effects(), [ClientEffect::Interaction(InteractionEffect::Sleep { .. })]));
    assert_ne!(epoch(&client), old_epoch);
    assert!(!client.state().interactions.sessions["session"].connected);
    assert!(client.respond_interaction(old_identity.clone(), answer()).is_empty());
    apply(&mut client, InteractionEvent::Snapshot { session_id: "session".into(), epoch: old_epoch, snapshot: snapshot("retired", 20, vec![]) });
    apply(&mut client, InteractionEvent::Failed { session_id: "session".into(), epoch: old_epoch, error: ClientError::waltr_login_required("late login error") });
    assert_eq!(client.state().access.status, crate::AccessStatus::Authenticated);
    let current_epoch = epoch(&client);
    let update = apply(&mut client, InteractionEvent::Retry { session_id: "session".into(), epoch: current_epoch });
    assert!(matches!(update.effects(), [ClientEffect::Interaction(InteractionEffect::Watch { .. })]));
    observe(&mut client, snapshot("second", 0, vec![question()]));
    assert!(apply(&mut client, InteractionEvent::Responded { identity: old_identity, epoch: old_epoch, error: None }).is_empty());
    assert_eq!(identity(&client).generation, "second");
}

#[test]
fn navigation_releases_observation_not_work_and_expiry_clears_protected_state() {
    let mut client = ready();
    client.retain_interactions("session");
    client.retain_interactions("session");
    observe(&mut client, snapshot("generation", 1, vec![question()]));
    let old_epoch = epoch(&client);
    assert!(client.release_interactions("session").is_empty());
    assert_eq!(client.state().interactions.sessions["session"].pending.len(), 1);
    assert!(client.release_interactions("session").is_empty());
    assert!(!client.state().interactions.sessions.contains_key("session"));
    client.retain_interactions("session");
    assert_ne!(epoch(&client), old_epoch);
    apply(&mut client, InteractionEvent::Snapshot { session_id: "session".into(), epoch: old_epoch, snapshot: snapshot("generation", 1, vec![question()]) });
    assert!(client.state().interactions.sessions["session"].pending.is_empty());
    observe(&mut client, snapshot("generation", 2, vec![question()]));
    let old_epoch = epoch(&client);
    client.expire_waltr_access("expired");
    assert!(client.state().interactions.sessions.is_empty());
    assert_eq!(client.state().interactions.watches["session"].interests, 1);
    apply(&mut client, InteractionEvent::Snapshot { session_id: "session".into(), epoch: old_epoch, snapshot: snapshot("generation", 3, vec![question()]) });
    assert!(client.state().interactions.sessions.is_empty());
    client.logout_waltr();
    assert!(client.state().interactions.watches.is_empty());
}

#[test]
fn invalid_or_stale_response_retains_request_and_refresh_never_resubmits() {
    let mut client = ready();
    client.retain_interactions("session");
    observe(&mut client, snapshot("generation", 1, vec![question()]));
    let identity = identity(&client);
    assert!(client.respond_interaction(identity.clone(), InteractionResponse::Question { answers: vec![vec!["No".into()]] }).is_empty());
    assert!(client.state().interactions.sessions["session"].pending[0].error.is_some());
    client.respond_interaction(identity.clone(), answer());
    let current_epoch = epoch(&client);
    let update = apply(&mut client, InteractionEvent::Responded { identity, epoch: current_epoch, error: Some(ClientError::new("FAILED_PRECONDITION: stale")) });
    assert!(matches!(update.effects(), [ClientEffect::Interaction(InteractionEffect::Refresh { .. })]));
    let pending = &client.state().interactions.sessions["session"].pending[0];
    assert!(!pending.responding);
    assert!(pending.error.as_ref().unwrap().contains("stale"));
    assert_eq!(client.state().interactions.sessions["session"].pending.len(), 1);
}

#[test]
fn refresh_started_before_watch_update_cannot_restore_old_generation() {
    let mut client = ready();
    client.retain_interactions("session");
    observe(&mut client, snapshot("first", 1, vec![question()]));
    let current_epoch = epoch(&client);
    observe(&mut client, snapshot("second", 0, vec![]));
    apply(&mut client, InteractionEvent::Refreshed { session_id: "session".into(), epoch: current_epoch, baseline: Some(("first".into(), 1)), snapshot: snapshot("first", 2, vec![question()]) });
    assert_eq!(client.state().interactions.sessions["session"].generation.as_deref(), Some("second"));
    assert!(client.state().interactions.sessions["session"].pending.is_empty());
}

#[test]
fn retired_response_and_refresh_failures_cannot_clear_current_access_or_send_queued_response() {
    let mut client = ready();
    client.retain_interactions("session");
    observe(&mut client, snapshot("first", 1, vec![question()]));
    let old_identity = identity(&client);
    let current_epoch = epoch(&client);
    let update = client.respond_interaction(old_identity.clone(), answer());
    let [ClientEffect::Interaction(effect)] = update.effects() else { panic!("response"); };
    observe(&mut client, snapshot("second", 0, vec![question()]));
    assert!(!effect_is_current(client.state(), effect));
    assert!(apply(&mut client, InteractionEvent::Responded { identity: old_identity, epoch: current_epoch, error: Some(ClientError::waltr_login_required("retired response")) }).is_empty());
    assert!(apply(&mut client, InteractionEvent::RefreshFailed { session_id: "session".into(), epoch: current_epoch, baseline: Some(("first".into(), 1)), error: ClientError::waltr_login_required("retired refresh") }).is_empty());
    assert_eq!(client.state().access.status, crate::AccessStatus::Authenticated);
    assert_eq!(identity(&client).generation, "second");
}

#[test]
fn permission_uses_only_selected_authoritative_suggestions_without_local_grants() {
    let mut client = ready();
    client.retain_interactions("session");
    let suggestion = serde_json::json!({"operation":"Read","resource":{"FilesystemPathPrefix":"/workspace"}});
    let request = pending(wire::pending_interaction::Request::Permission(wire::PermissionInteraction { message: "Read workspace?".into(), identity: "read_file".into(), claims_json: "[]".into(), suggested_patterns_json: serde_json::json!([suggestion]).to_string() }));
    observe(&mut client, snapshot("generation", 1, vec![request]));
    let identity = identity(&client);
    assert!(client.respond_interaction(identity.clone(), InteractionResponse::Permission { decision: InteractionPermissionDecision::AllowOnce, suggestion_indices: vec![0], message: None }).is_empty());
    let update = client.respond_interaction(identity, InteractionResponse::Permission { decision: InteractionPermissionDecision::AllowAlways, suggestion_indices: vec![0], message: None });
    let [ClientEffect::Interaction(InteractionEffect::Respond { request, .. })] = update.effects() else { panic!("permission response"); };
    let Some(wire::respond_interaction_request::Response::Permission(response)) = &request.response else { panic!("permission"); };
    assert_eq!(response.decision, 2);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&response.patterns_json).unwrap(), serde_json::json!([suggestion]));
    assert_eq!(client.state().interactions.sessions["session"].pending.len(), 1);
}

#[test]
fn forms_preserve_labels_defaults_required_enum_and_bounds() {
    let fields = parse_form(r#"{"type":"object","required":["name","age","roles"],"properties":{"name":{"type":"string","minLength":2,"maxLength":8,"default":"Ada"},"age":{"type":"integer","minimum":1,"maximum":120},"enabled":{"type":"boolean","default":false},"email":{"type":"string","format":"email"},"color":{"type":"string","enum":["red","blue"],"enumNames":["Red","Blue"]},"roles":{"type":"array","minItems":1,"maxItems":2,"items":{"anyOf":[{"const":"reader","title":"Reader"},{"const":"writer","title":"Writer"}]}}}}"#).unwrap();
    assert_eq!(fields.iter().find(|f| f.name == "name").unwrap().default, Some(InteractionFormValue::String("Ada".into())));
    assert_eq!(fields.iter().find(|f| f.name == "roles").unwrap().choice_titles["reader"], "Reader");
    let mut values = BTreeMap::from([("name".into(), InteractionFormValue::String("Ada".into())), ("age".into(), InteractionFormValue::Number(42.0)), ("roles".into(), InteractionFormValue::Strings(vec!["reader".into()]))]);
    assert!(validate_form(&fields, &values).is_ok());
    values.insert("age".into(), InteractionFormValue::Number(42.5));
    assert!(validate_form(&fields, &values).is_err());
    values.insert("age".into(), InteractionFormValue::Number(121.0));
    assert!(validate_form(&fields, &values).is_err());
    values.insert("age".into(), InteractionFormValue::Number(42.0));
    values.insert("roles".into(), InteractionFormValue::Strings(vec!["admin".into()]));
    assert!(validate_form(&fields, &values).is_err());
    values.remove("roles");
    assert!(validate_form(&fields, &values).is_err());
    assert!(parse_form(r#"{"type":"object","properties":{"nested":{"type":"object"}}}"#).is_err());
    assert!(parse_form(r#"{"type":"object","properties":{"x":{"type":"string","pattern":".*"}}}"#).is_err());
    assert!(parse_form(r#"{"type":"object","properties":{"x":{"type":"number","minimum":2,"maximum":1}}}"#).is_err());
    let titled = parse_form(r#"{"type":"object","properties":{"choice":{"type":"string","oneOf":[{"const":"a","title":"Alpha"}],"default":"a"}}}"#).unwrap();
    assert_eq!(titled[0].choice_titles["a"], "Alpha");
}

#[test]
fn typed_form_and_url_consent_responses_do_not_claim_external_completion() {
    let identity = InteractionIdentity { session_id: "session".into(), generation: "generation".into(), id: "pending".into(), operation_id: "operation".into() };
    let fields = parse_form(r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#).unwrap();
    let view = InteractionRequestView::Form { message: "Confirm".into(), fields };
    let response = response_wire(&identity, &view, None, InteractionResponse::Form { action: InteractionElicitationAction::Accept, values: BTreeMap::from([("ok".into(), InteractionFormValue::Boolean(false))]) }).unwrap();
    let Some(wire::respond_interaction_request::Response::Form(form)) = response.response else { panic!("form"); };
    assert_eq!(form.content_json.as_deref(), Some("{\"ok\":false}"));
    let full_url = "https://example.test/authorize?scope=all#consent";
    let view = InteractionRequestView::UrlConsent { message: "Review URL".into(), url: full_url.into() };
    let response = response_wire(&identity, &view, None, InteractionResponse::UrlConsent { action: InteractionElicitationAction::Accept }).unwrap();
    let Some(wire::respond_interaction_request::Response::UrlConsent(consent)) = response.response else { panic!("URL consent"); };
    assert_eq!(consent.action, 1);
    assert!(consent.content_json.is_none());
    assert!(matches!(view, InteractionRequestView::UrlConsent { url, .. } if url == full_url));
    let unsupported = InteractionRequestView::Unsupported { message: "Nested schema".into(), form: true };
    assert!(response_wire(&identity, &unsupported, None, InteractionResponse::Form { action: InteractionElicitationAction::Accept, values: BTreeMap::new() }).is_err());
    assert!(response_wire(&identity, &unsupported, None, InteractionResponse::Form { action: InteractionElicitationAction::Decline, values: BTreeMap::new() }).is_ok());
}

#[test]
fn question_multiple_custom_values_and_atomic_answer_count_are_validated() {
    let identity = InteractionIdentity { session_id: "s".into(), generation: "g".into(), id: "i".into(), operation_id: "o".into() };
    let view = InteractionRequestView::Question { questions: vec![InteractionQuestionView { question: "Choose".into(), header: "Q".into(), options: vec![InteractionOptionView { label: "A".into(), description: "First".into() }], multiple: true, custom: true }] };
    assert!(response_wire(&identity, &view, None, InteractionResponse::Question { answers: vec![vec!["A".into(), "custom".into()]] }).is_ok());
    assert!(response_wire(&identity, &view, None, InteractionResponse::Question { answers: vec![vec!["A".into(), "A".into()]] }).is_err());
    assert!(response_wire(&identity, &view, None, InteractionResponse::Question { answers: vec![] }).is_err());
}

#[test]
fn all_permission_and_elicitation_decisions_remain_explicit() {
    let identity = InteractionIdentity { session_id: "s".into(), generation: "g".into(), id: "i".into(), operation_id: "o".into() };
    let p = wire::PendingInteraction { id: "i".into(), operation_id: "o".into(), session_id: "s".into(), request: Some(wire::pending_interaction::Request::Permission(wire::PermissionInteraction { message: "Approve".into(), identity: "tool".into(), claims_json: "[]".into(), suggested_patterns_json: "[]".into() })) };
    let view = request_view(&p);
    let snapshot = snapshot("g", 1, vec![p]);
    for (decision, expected) in [(InteractionPermissionDecision::AllowOnce, 1), (InteractionPermissionDecision::AllowAlways, 2), (InteractionPermissionDecision::Deny, 3), (InteractionPermissionDecision::CancelWork, 4)] {
        let response = response_wire(&identity, &view, Some(&snapshot), InteractionResponse::Permission { decision, suggestion_indices: vec![], message: Some("choice".into()) }).unwrap();
        let Some(wire::respond_interaction_request::Response::Permission(response)) = response.response else { panic!("permission"); };
        assert_eq!(response.decision, expected);
    }
    let view = InteractionRequestView::UrlConsent { message: "Consent only".into(), url: "https://example.test".into() };
    for (action, expected) in [(InteractionElicitationAction::Accept, 1), (InteractionElicitationAction::Decline, 2), (InteractionElicitationAction::Cancel, 3)] {
        let response = response_wire(&identity, &view, None, InteractionResponse::UrlConsent { action }).unwrap();
        let Some(wire::respond_interaction_request::Response::UrlConsent(response)) = response.response else { panic!("consent"); };
        assert_eq!(response.action, expected);
        assert!(response.content_json.is_none());
    }
}

#[test]
fn status_failure_parks_reconnect_and_cancelled_owner_is_authoritative_removal() {
    let mut client = ready();
    client.retain_interactions("session");
    observe(&mut client, snapshot("g", 1, vec![question()]));
    let old_epoch = epoch(&client);
    apply(&mut client, InteractionEvent::Failed { session_id: "session".into(), epoch: old_epoch, error: ClientError::new("offline") });
    client.apply(ClientEvent::Status(StatusEvent::WatchFailed(ClientError::new("status offline"))));
    let epoch = epoch(&client);
    assert!(apply(&mut client, InteractionEvent::Retry { session_id: "session".into(), epoch }).is_empty());
    client.apply(ClientEvent::Status(StatusEvent::WatchChanged(SystemStatus { ready: true })));
    assert!(matches!(client.maintain_watches().effects(), [ClientEffect::Interaction(InteractionEffect::Watch { .. })]));
    observe(&mut client, snapshot("g", 2, vec![]));
    assert!(client.state().interactions.sessions["session"].pending.is_empty());
}

#[test]
fn supported_string_formats_validate_submitted_values_and_defaults() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        ("email", &["person@example.test", "first.last+tag@sub.example.test", "名@example.test"], &["", "person", "person@example", "@example.test", "person@.test", "person@example.", "person@@example.test", "person @example.test", "person\n@example.test", "person@exam\u{feff}ple.test"]),
        ("uri", &["https://example.test/path?query=1#fragment", "mailto:person@example.test", "urn:isbn:9780140328721", "web+demo:opaque", "file:///workspace/file"], &["", "/relative/path", "example.test/path", "1invalid:opaque", "https://", "https://[invalid]"]),
        ("date", &["2024-02-29", "2000-02-29", "1900-02-28", "0000-02-29", "9999-12-31"], &["2023-02-29", "1900-02-29", "2024-04-31", "2024-00-01", "2024-13-01", "2024-01-00", "2024-01-32", "2024-1-01", "24-01-01", "2024-01-01Z", " 2024-01-01", "２０２４-01-01"]),
        ("date-time", &["2024-02-29T23:59:59Z", "2024-02-29t23:59:59z", "2024-01-01T00:00:00.123456789Z", "2024-01-01T00:00:00+05:30", "2024-01-01T00:00:00-23:59", "2000-02-29T12:30:45.0+00:00"], &["2023-02-29T12:00:00Z", "2024-04-31T12:00:00Z", "2024-01-01T24:00:00Z", "2024-01-01T23:60:00Z", "2024-01-01T23:59:60Z", "2024-01-01T12:00:00+24:00", "2024-01-01T12:00:00+01:60", "2024-01-01T12:00:00", "2024-01-01 12:00:00Z", "2024-01-01T12:00Z", "2024-01-01T1:00:00Z", "2024-01-01T12:00:00.Z", "2024-01-01T12:00:00.12", "2024-01-01T12:00:00+0100", "2024-01-01T12:00:00Zextra", "2024-01-01T12:00:00Z\n"]),
    ];
    let identity = InteractionIdentity { session_id: "session".into(), generation: "generation".into(), id: "pending".into(), operation_id: "operation".into() };
    for &(format, valid, invalid) in cases {
        let schema = serde_json::json!({"type":"object","required":["value"],"properties":{"value":{"type":"string","format":format}}});
        let fields = parse_form(&schema.to_string()).unwrap();
        let view = InteractionRequestView::Form { message: "Enter a value".into(), fields };
        for (values, expected) in [(valid, true), (invalid, false)] {
            for &value in values {
                let response = InteractionResponse::Form { action: InteractionElicitationAction::Accept, values: BTreeMap::from([("value".into(), InteractionFormValue::String(value.into()))]) };
                assert_eq!(response_wire(&identity, &view, None, response).is_ok(), expected, "submitted {format}: {value:?}");
                let mut with_default = schema.clone();
                with_default["properties"]["value"]["default"] = serde_json::json!(value);
                assert_eq!(parse_form(&with_default.to_string()).is_ok(), expected, "default {format}: {value:?}");
            }
        }
    }
}

#[test]
fn string_format_validation_preserves_other_constraints_and_optional_omission() {
    let fields = parse_form(r#"{"type":"object","properties":{"email":{"type":"string","format":"email","minLength":5,"maxLength":30,"enum":["person@example.test"]}}}"#).unwrap();
    assert!(validate_form(&fields, &BTreeMap::new()).is_ok());
    assert!(validate_form(&fields, &BTreeMap::from([("email".into(), InteractionFormValue::String("person@example.test".into()))])).is_ok());
    assert!(validate_form(&fields, &BTreeMap::from([("email".into(), InteractionFormValue::String("other@example.test".into()))])).is_err());
    assert!(!valid_string_format("person@example.test", "unknown"));
}
