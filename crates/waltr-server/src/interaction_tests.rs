use super::*;
use serde_json::json;

fn binding() -> OperationBinding {
    OperationBinding {
        operation_id: "durable-operation".into(),
        destination: InteractionDestination::Session(waltr_agent::SessionId::new("session")),
        cancellation: CancellationToken::new(),
    }
}
fn question() -> Interaction {
    Interaction::Question {
        input: serde_json::from_value(json!({"questions":[{
        "question":"Choose", "header":"Choice", "options":[{"label":"A","description":"A"}],
        "multiple":false,"custom":false }]}))
        .unwrap(),
    }
}
fn answer() -> InteractionResponse {
    InteractionResponse::Question(vec![vec!["A".into()]])
}

async fn pending(broker: &InteractionBroker, binding: &OperationBinding) -> PendingInteraction {
    let InteractionDestination::Session(session) = &binding.destination else {
        panic!()
    };
    let mut watch = broker.subscribe(session.clone());
    loop {
        let snapshot = watch.next().await.unwrap();
        if let Some(request) = snapshot.pending.into_iter().next() {
            return request;
        }
    }
}

#[tokio::test]
async fn authorized_viewers_share_one_request_and_one_winner() {
    let broker = InteractionBroker::new();
    let operation = binding();
    let b = broker.clone();
    let op = operation.clone();
    let owner = tokio::spawn(async move { b.interact(&op, question()).await });
    let request = pending(&broker, &operation).await;
    let session = waltr_agent::SessionId::new("session");
    let a = broker.snapshot(&session);
    let b = broker.subscribe(session.clone()).next().await.unwrap();
    assert_eq!(a.pending[0].id, b.pending[0].id);
    assert_eq!(
        broker.respond(
            &session,
            &a.generation,
            &request.id,
            &request.operation_id,
            answer()
        ),
        Ok(())
    );
    assert_eq!(
        broker.respond(
            &session,
            &a.generation,
            &request.id,
            &request.operation_id,
            answer()
        ),
        Err(InteractionError::Stale)
    );
    assert!(matches!(
        owner.await.unwrap(),
        Ok(InteractionResponse::Question(_))
    ));
    assert!(broker.snapshot(&session).pending.is_empty());
}

#[tokio::test]
async fn invalid_stale_or_wrong_binding_does_not_consume() {
    let broker = InteractionBroker::new();
    let operation = binding();
    let b = broker.clone();
    let op = operation.clone();
    let owner = tokio::spawn(async move { b.interact(&op, question()).await });
    let request = pending(&broker, &operation).await;
    let session = waltr_agent::SessionId::new("session");
    let snapshot = broker.snapshot(&session);
    assert_eq!(
        broker.respond(
            &session,
            &snapshot.generation,
            &request.id,
            "other-operation",
            answer()
        ),
        Err(InteractionError::Stale)
    );
    assert_eq!(
        broker.respond(
            &waltr_agent::SessionId::new("other"),
            &snapshot.generation,
            &request.id,
            &request.operation_id,
            answer()
        ),
        Err(InteractionError::Stale)
    );
    assert_eq!(
        broker.respond(
            &session,
            &snapshot.generation,
            &request.id,
            &request.operation_id,
            InteractionResponse::Question(vec![vec!["invalid".into()]])
        ),
        Err(InteractionError::Invalid)
    );
    assert_eq!(broker.snapshot(&session).pending.len(), 1);
    operation.cancellation.cancel();
    assert_eq!(
        owner.await.unwrap().unwrap_err(),
        InteractionError::Cancelled
    );
}

#[tokio::test]
async fn snapshot_subscription_reconnect_and_owner_drop_never_replay() {
    let broker = InteractionBroker::new();
    let operation = binding();
    let session = waltr_agent::SessionId::new("session");
    let mut watch = broker.subscribe(session.clone());
    assert!(watch.next().await.unwrap().pending.is_empty());
    let b = broker.clone();
    let op = operation.clone();
    let owner = tokio::spawn(async move { b.interact(&op, question()).await });
    let request = pending(&broker, &operation).await;
    assert_eq!(watch.next().await.unwrap().pending[0].id, request.id);
    drop(watch);
    assert_eq!(
        broker
            .subscribe(session.clone())
            .next()
            .await
            .unwrap()
            .pending[0]
            .id,
        request.id
    );
    owner.abort();
    drop(owner.await);
    assert!(broker.snapshot(&session).pending.is_empty());
    let restarted = InteractionBroker::new();
    assert_ne!(
        broker.snapshot(&session).generation,
        restarted.snapshot(&session).generation
    );
    assert!(restarted.snapshot(&session).pending.is_empty());
    assert_eq!(
        restarted.respond(
            &session,
            &broker.snapshot(&session).generation,
            &request.id,
            &request.operation_id,
            answer()
        ),
        Err(InteractionError::Stale)
    );
}

#[tokio::test]
async fn sessionless_without_route_fails_without_broadcast() {
    let broker = InteractionBroker::new();
    let mut operation = binding();
    operation.destination = InteractionDestination::Unavailable;
    assert_eq!(
        broker.interact(&operation, question()).await.unwrap_err(),
        InteractionError::Unavailable
    );
    assert!(
        broker
            .snapshot(&waltr_agent::SessionId::new("session"))
            .pending
            .is_empty()
    );
}

#[test]
fn form_and_url_have_distinct_validated_response_semantics() {
    let form = Interaction::Form { request: mcp::McpElicitationRequest::parse(&json!({
        "message":"Name", "requestedSchema":{"type":"object", "properties":{"name":{"type":"string"}}, "required":["name"]}
    })).unwrap() };
    let accept = mcp::McpElicitationResponse {
        action: mcp::McpElicitationAction::Accept,
        content: None,
    };
    assert_eq!(
        form.validate_response(&InteractionResponse::Form(accept.clone())),
        Err(InteractionError::Invalid)
    );
    let url = Interaction::UrlConsent {
        request: mcp::McpElicitationRequest::Url {
            message: "Consent to external navigation".into(),
            url: "https://example.com/consent".into(),
        },
    };
    assert!(url.validate().is_ok());
    assert!(
        url.validate_response(&InteractionResponse::UrlConsent(accept.clone()))
            .is_ok()
    );
    assert_eq!(
        url.validate_response(&InteractionResponse::Form(accept)),
        Err(InteractionError::Invalid)
    );
    let unsafe_url = Interaction::UrlConsent {
        request: mcp::McpElicitationRequest::Url {
            message: "unsafe".into(),
            url: "https://user:secret@example.com/".into(),
        },
    };
    assert_eq!(unsafe_url.validate(), Err(InteractionError::Invalid));
}

#[tokio::test]
async fn concurrent_valid_responders_have_exactly_one_winner() {
    let broker = InteractionBroker::new();
    let operation = binding();
    let b = broker.clone();
    let op = operation.clone();
    let owner = tokio::spawn(async move { b.interact(&op, question()).await });
    let request = pending(&broker, &operation).await;
    let session = waltr_agent::SessionId::new("session");
    let snapshot = broker.snapshot(&session);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let respond = |broker: InteractionBroker, barrier: Arc<tokio::sync::Barrier>| {
        let session = session.clone();
        let generation = snapshot.generation.clone();
        let request = request.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            broker.respond(
                &session,
                &generation,
                &request.id,
                &request.operation_id,
                answer(),
            )
        })
    };
    let a = respond(broker.clone(), barrier.clone());
    let b = respond(broker.clone(), barrier);
    let results = [a.await.unwrap(), b.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(InteractionError::Stale))
            .count(),
        1
    );
    assert!(owner.await.unwrap().is_ok());
}

#[test]
fn reusable_permission_patterns_must_be_nonempty_declared_scopes() {
    let request = Interaction::Permission {
        message: "Allow?".into(),
        identity: "tool".into(),
        claims: vec![],
        patterns: vec![],
    };
    assert_eq!(
        request.validate_response(&InteractionResponse::Permission(
            PermissionResponse::AllowAlways { patterns: vec![] }
        )),
        Err(InteractionError::Invalid)
    );
    assert!(
        request
            .validate_response(&InteractionResponse::Permission(
                PermissionResponse::AllowOnce
            ))
            .is_ok()
    );
}
