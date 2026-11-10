use super::*;
use waltr_client::{ClientRuntime, InteractionOptionView, InteractionQuestionView, SessionInteractionsView};

fn pending(id: &str, request: Request) -> PendingInteractionView {
    PendingInteractionView {
        identity: InteractionIdentity { session_id: "chat".into(), generation: "generation-1".into(), id: id.into(), operation_id: format!("operation-{id}") },
        request, responding: false, error: None,
    }
}
fn permission() -> Request {
    Request::Permission { message: "Run command?".into(), identity: "shell".into(), claims: vec!["claim".into()], suggestions: vec!["scope-one".into(), "scope-two".into()] }
}
fn setup(request: Request) -> (SessionsSection, ClientSnapshot) {
    let mut snapshot = ClientRuntime::for_endpoint("http://127.0.0.1:0").snapshot();
    snapshot.interactions.insert("chat".into(), SessionInteractionsView { generation: Some("generation-1".into()), connected: true, pending: vec![pending("one", request)], ..Default::default() });
    let mut section = SessionsSection::new();
    section.detail_session_id = Some("chat".into());
    section.prompt_draft.reconcile(&snapshot, Some("chat"));
    (section, snapshot)
}
fn press(section: &mut SessionsSection, snapshot: &ClientSnapshot, key: KeyCode) -> SectionAction {
    section.handle_prompt_key(KeyEvent::new(key, KeyModifiers::NONE), Some(snapshot))
}
fn response(action: SectionAction) -> (InteractionIdentity, Response) {
    let SectionAction::ClientCommand(ClientCommand::RespondInteraction { identity, response }) = action else { panic!("expected typed interaction response"); };
    (identity, response)
}
fn field(name: &str, schema: Schema, default: Option<Value>) -> InteractionFormField {
    InteractionFormField { name: name.into(), title: Some(format!("Title {name}")), description: Some("Field help".into()), required: true, default, choice_titles: BTreeMap::new(), schema }
}

#[test]
fn permission_keyboard_decisions_copy_exact_identity_and_never_interrupt_separately() {
    for (key, decision) in [('a', Decision::AllowOnce), ('d', Decision::Deny), ('c', Decision::CancelWork)] {
        let (mut section, snapshot) = setup(permission());
        let (identity, r) = response(press(&mut section, &snapshot, KeyCode::Char(key)));
        assert_eq!(identity, snapshot.interactions["chat"].pending[0].identity);
        assert_eq!(r, Response::Permission { decision, suggestion_indices: vec![], message: None });
        assert!(matches!(press(&mut section, &snapshot, KeyCode::Char(key)), SectionAction::Continue));
        assert_eq!(snapshot.interactions["chat"].pending.len(), 1);
    }
}

#[test]
fn reusable_permission_requires_selected_server_suggestions() {
    let (mut section, snapshot) = setup(permission());
    assert!(matches!(press(&mut section, &snapshot, KeyCode::Char('A')), SectionAction::Continue));
    press(&mut section, &snapshot, KeyCode::Down);
    press(&mut section, &snapshot, KeyCode::Char(' '));
    let (_, r) = response(press(&mut section, &snapshot, KeyCode::Char('A')));
    assert_eq!(r, Response::Permission { decision: Decision::AllowAlways, suggestion_indices: vec![1], message: None });
}

#[test]
fn questions_support_single_multiple_custom_and_explicit_all_question_submit() {
    let option = |s: &str| InteractionOptionView { label: s.into(), description: format!("About {s}") };
    let (mut section, snapshot) = setup(Request::Question { questions: vec![
        InteractionQuestionView { header: "Single".into(), question: "Choose".into(), options: vec![option("a"), option("b")], multiple: false, custom: false },
        InteractionQuestionView { header: "Multiple".into(), question: "Choose several".into(), options: vec![option("x"), option("y")], multiple: true, custom: true },
    ] });
    assert!(matches!(press(&mut section, &snapshot, KeyCode::F(7)), SectionAction::Continue));
    press(&mut section, &snapshot, KeyCode::Enter);
    press(&mut section, &snapshot, KeyCode::Down);
    press(&mut section, &snapshot, KeyCode::Enter);
    assert_eq!(section.prompt_draft.answers[0], vec!["b"]);
    press(&mut section, &snapshot, KeyCode::Tab);
    press(&mut section, &snapshot, KeyCode::Enter);
    press(&mut section, &snapshot, KeyCode::Down);
    press(&mut section, &snapshot, KeyCode::Enter);
    for c in "custom".chars() { press(&mut section, &snapshot, KeyCode::Char(c)); }
    press(&mut section, &snapshot, KeyCode::Enter);
    let (_, r) = response(press(&mut section, &snapshot, KeyCode::F(7)));
    assert_eq!(r, Response::Question { answers: vec![vec!["b".into()], vec!["x".into(), "y".into(), "custom".into()]] });
}

#[test]
fn flat_form_keyboard_preserves_types_defaults_titles_and_choice_values() {
    let mut string = field("color", Schema::String { choices: Some(vec!["red".into(), "blue".into()]), min_length: Some(3), max_length: Some(4), format: None }, None);
    string.choice_titles.insert("blue".into(), "Blue title".into());
    let fields = vec![string,
        field("count", Schema::Number { integer: true, minimum: Some(1.0), maximum: Some(3.0) }, None),
        field("enabled", Schema::Boolean, Some(Value::Boolean(false))),
        field("tags", Schema::StringArray { choices: vec!["x".into(), "y".into()], min_items: Some(1), max_items: Some(2) }, None),
    ];
    let (mut section, snapshot) = setup(Request::Form { message: "Configure".into(), fields });
    assert!(matches!(press(&mut section, &snapshot, KeyCode::F(7)), SectionAction::Continue));
    press(&mut section, &snapshot, KeyCode::Down);
    press(&mut section, &snapshot, KeyCode::Enter);
    press(&mut section, &snapshot, KeyCode::Tab);
    press(&mut section, &snapshot, KeyCode::Char('2'));
    press(&mut section, &snapshot, KeyCode::Tab);
    press(&mut section, &snapshot, KeyCode::Char(' '));
    press(&mut section, &snapshot, KeyCode::Tab);
    press(&mut section, &snapshot, KeyCode::Char(' '));
    let (_, r) = response(press(&mut section, &snapshot, KeyCode::F(7)));
    assert_eq!(r, Response::Form { action: Action::Accept, values: BTreeMap::from([
        ("color".into(), Value::String("blue".into())), ("count".into(), Value::Number(2.0)),
        ("enabled".into(), Value::Boolean(true)), ("tags".into(), Value::Strings(vec!["x".into()])),
    ]) });
}

#[test]
fn form_admission_rejects_length_bounds_noninteger_nonfinite_choices_counts_and_formats() {
    let cases = [
        (Schema::String { choices: None, min_length: Some(2), max_length: Some(3), format: None }, "x"),
        (Schema::String { choices: None, min_length: None, max_length: Some(1), format: None }, "xx"),
        (Schema::String { choices: Some(vec!["yes".into()]), min_length: None, max_length: None, format: None }, "no"),
        (Schema::String { choices: None, min_length: None, max_length: None, format: Some("email".into()) }, "invalid"),
        (Schema::Number { integer: true, minimum: None, maximum: None }, "1.5"),
        (Schema::Number { integer: false, minimum: Some(1.0), maximum: Some(2.0) }, "3"),
        (Schema::Number { integer: false, minimum: None, maximum: None }, "NaN"),
    ];
    for (schema, text) in cases {
        let fields = vec![field("value", schema, None)];
        let mut d = PromptDraft::new(); d.fields.insert("value".into(), text.into());
        assert!(form_values(&fields, &d).is_err());
    }
    let fields = vec![field("value", Schema::StringArray { choices: vec!["x".into()], min_items: Some(1), max_items: Some(1) }, None)];
    for values in [vec![], vec!["invalid".into()], vec!["x".into(), "x".into()]] {
        let mut d = PromptDraft::new(); d.arrays.insert("value".into(), values);
        assert!(form_values(&fields, &d).is_err());
    }
    assert!(valid_format("2024-02-29", Some("date")));
    assert!(!valid_format("2025-02-29", Some("date")));
    assert!(valid_format("2026-10-28T10:20:30Z", Some("date-time")));
    assert!(!valid_format("2026-10-28T25:20:30Z", Some("date-time")));
    assert!(valid_format("https://example.com/complete", Some("uri")));
    assert!(!valid_format("not a URL", Some("uri")));
}

#[test]
fn elicitation_decline_cancel_and_unsupported_never_pretend_acceptance() {
    for request in [Request::Form { message: "Form".into(), fields: vec![] }, Request::Unsupported { message: "Nested schema".into(), form: true }] {
        for (key, action) in [(KeyCode::F(8), Action::Decline), (KeyCode::F(9), Action::Cancel)] {
            let (mut section, snapshot) = setup(request.clone());
            let (_, r) = response(press(&mut section, &snapshot, key));
            assert_eq!(r, Response::Form { action, values: BTreeMap::new() });
        }
    }
    let (mut section, snapshot) = setup(Request::Unsupported { message: "Nested schema".into(), form: true });
    assert!(matches!(press(&mut section, &snapshot, KeyCode::F(7)), SectionAction::Continue));
}

#[test]
fn url_keyboard_consent_is_typed_and_navigation_never_proves_completion() {
    for (key, action) in [(KeyCode::F(7), Action::Accept), (KeyCode::F(8), Action::Decline), (KeyCode::F(9), Action::Cancel)] {
        let (mut section, snapshot) = setup(Request::UrlConsent { message: "Authorize".into(), url: "https://example.com/complete?token=opaque".into() });
        for key in [KeyCode::Tab, KeyCode::Enter, KeyCode::Right] { assert!(matches!(press(&mut section, &snapshot, key), SectionAction::Continue)); }
        let (_, r) = response(press(&mut section, &snapshot, key));
        assert_eq!(r, Response::UrlConsent { action });
    }
}

#[test]
fn authoritative_resolution_generation_and_session_changes_discard_obsolete_drafts() {
    let (mut section, mut snapshot) = setup(permission());
    snapshot.interactions.get_mut("chat").unwrap().pending.push(pending("two", permission()));
    section.handle_prompt_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL), Some(&snapshot));
    assert_eq!(section.prompt_draft.identity.as_ref().unwrap().id, "two");
    press(&mut section, &snapshot, KeyCode::Char(' '));
    snapshot.interactions.get_mut("chat").unwrap().pending.remove(0);
    section.prompt_draft.reconcile(&snapshot, Some("chat"));
    assert_eq!(section.prompt_draft.request_index, 0);
    assert_eq!(section.prompt_draft.selected, BTreeSet::from([0]));
    snapshot.interactions.get_mut("chat").unwrap().pending[0].identity.generation = "generation-2".into();
    section.prompt_draft.reconcile(&snapshot, Some("chat"));
    assert!(section.prompt_draft.selected.is_empty());
    section.prompt_draft.reconcile(&snapshot, Some("another-session"));
    assert!(section.prompt_draft.identity.is_none());
}

#[test]
fn busy_disconnected_and_stale_views_disable_responses_and_errors_allow_explicit_retry() {
    let (mut section, mut snapshot) = setup(permission());
    snapshot.interactions.get_mut("chat").unwrap().connected = false;
    assert!(matches!(press(&mut section, &snapshot, KeyCode::Char('a')), SectionAction::Continue));
    snapshot.interactions.get_mut("chat").unwrap().connected = true;
    snapshot.interactions.get_mut("chat").unwrap().pending[0].responding = true;
    assert!(matches!(press(&mut section, &snapshot, KeyCode::Char('a')), SectionAction::Continue));
    snapshot.interactions.get_mut("chat").unwrap().pending[0].responding = false;
    response(press(&mut section, &snapshot, KeyCode::Char('a')));
    snapshot.interactions.get_mut("chat").unwrap().pending[0].error = Some("Stale response; refresh".into());
    response(press(&mut section, &snapshot, KeyCode::Char('d')));
    assert!(matches!(press(&mut section, &snapshot, KeyCode::Char('d')), SectionAction::Continue));
    snapshot.interactions.get_mut("chat").unwrap().pending.clear();
    assert!(matches!(press(&mut section, &snapshot, KeyCode::Char('a')), SectionAction::Continue));
}

#[test]
fn rendering_shows_full_url_consent_semantics_busy_and_observation_errors() {
    let (section, mut snapshot) = setup(Request::UrlConsent { message: "Authorize".into(), url: "https://example.com/complete?opaque=value".into() });
    let view = snapshot.interactions.get_mut("chat").unwrap();
    view.connected = false; view.error = Some("Observation lost".into());
    view.pending[0].responding = true; view.pending[0].error = Some("Outcome unknown".into());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| render_prompt(frame, frame.area(), &snapshot.interactions["chat"].pending[0], &section.prompt_draft, Some(&snapshot))).unwrap();
    let text = terminal.backend().to_string();
    for expected in ["https://example.com/complete?opaque=value", "Nothing opens automatically", "not browser completion", "Observation lost", "Outcome unknown", "authoritative resolution"] { assert!(text.contains(expected), "{text}"); }
}

#[test]
fn rendering_covers_permission_question_form_and_unsupported_actions() {
    let requests = [
        (permission(), vec!["Run command?", "Claim: claim", "scope-one", "cancel work"]),
        (Request::Question { questions: vec![InteractionQuestionView { header: "Heading".into(), question: "Question text".into(), options: vec![InteractionOptionView { label: "Option".into(), description: "Option help".into() }], multiple: true, custom: true }] }, vec!["Heading", "Question text", "Option help", "Type custom answer"]),
        (Request::Form { message: "Form message".into(), fields: vec![field("name", Schema::String { choices: None, min_length: Some(1), max_length: Some(20), format: None }, Some(Value::String("Default text".into())))] }, vec!["Form message", "Title name", "Field help", "Default text", "required", "F8 decline"]),
        (Request::Unsupported { message: "Nested schema".into(), form: true }, vec!["Unsupported request", "Nested schema", "Cannot accept", "F9 cancel"]),
    ];
    for (request, expected) in requests {
        let (section, snapshot) = setup(request);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| render_prompt(frame, frame.area(), &snapshot.interactions["chat"].pending[0], &section.prompt_draft, Some(&snapshot))).unwrap();
        let text = terminal.backend().to_string();
        for expected in expected { assert!(text.contains(expected), "{text}"); }
    }
}
