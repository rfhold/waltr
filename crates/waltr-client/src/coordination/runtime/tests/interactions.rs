use super::*;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use waltr::grpc::waltr::session::v1::{
    self as wire,
    interaction_service_server::{InteractionService, InteractionServiceServer},
};

#[derive(Clone)]
struct Service {
    snapshot: Arc<Mutex<wire::InteractionSnapshot>>,
    responses: Arc<Mutex<Vec<wire::RespondInteractionRequest>>>,
}
#[tonic::async_trait]
impl InteractionService for Service {
    async fn get_interactions(
        &self,
        request: tonic::Request<wire::GetInteractionsRequest>,
    ) -> Result<tonic::Response<wire::GetInteractionsResponse>, tonic::Status> {
        assert_eq!(request.into_inner().session_id, "session");
        Ok(tonic::Response::new(wire::GetInteractionsResponse {
            snapshot: Some(self.snapshot.lock().unwrap().clone()),
        }))
    }
    type WatchInteractionsStream = Pin<
        Box<
            dyn futures_core::Stream<Item = Result<wire::WatchInteractionsResponse, tonic::Status>>
                + Send,
        >,
    >;
    async fn watch_interactions(
        &self,
        request: tonic::Request<wire::WatchInteractionsRequest>,
    ) -> Result<tonic::Response<Self::WatchInteractionsStream>, tonic::Status> {
        assert_eq!(request.into_inner().session_id, "session");
        let snapshot = self.snapshot.lock().unwrap().clone();
        Ok(tonic::Response::new(Box::pin(
            futures_util::stream::once(async move {
                Ok(wire::WatchInteractionsResponse {
                    snapshot: Some(snapshot),
                })
            })
            .chain(futures_util::stream::pending()),
        )))
    }
    async fn respond_interaction(
        &self,
        request: tonic::Request<wire::RespondInteractionRequest>,
    ) -> Result<tonic::Response<wire::RespondInteractionResponse>, tonic::Status> {
        let request = request.into_inner();
        assert_eq!(
            (
                &*request.session_id,
                &*request.generation,
                &*request.id,
                &*request.operation_id
            ),
            (
                "session",
                "server-generation",
                "pending",
                "original-operation"
            )
        );
        let Some(wire::respond_interaction_request::Response::UrlConsent(consent)) =
            &request.response
        else {
            panic!("typed consent");
        };
        assert_eq!(consent.action, 1);
        assert!(consent.content_json.is_none());
        self.responses.lock().unwrap().push(request);
        let mut snapshot = self.snapshot.lock().unwrap();
        snapshot.pending.clear();
        snapshot.revision += 1;
        Ok(tonic::Response::new(wire::RespondInteractionResponse {}))
    }
}

#[tokio::test]
async fn native_watch_typed_response_and_get_refresh_use_one_shared_runtime() {
    let service = Service {
        snapshot: Arc::new(Mutex::new(wire::InteractionSnapshot {
            generation: "server-generation".into(),
            revision: 1,
            pending: vec![wire::PendingInteraction {
                id: "pending".into(),
                operation_id: "original-operation".into(),
                session_id: "session".into(),
                request: Some(wire::pending_interaction::Request::UrlConsent(
                    wire::UrlConsentInteraction {
                        message: "Review full URL".into(),
                        url: "https://example.test/consent?scope=all".into(),
                    },
                )),
            }],
        })),
        responses: Arc::new(Mutex::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let incoming = futures_util::stream::unfold(listener, |listener| async move {
        let accepted = listener.accept().await.map(|(socket, _)| socket);
        Some((accepted, listener))
    });
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server_service = service.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(InteractionServiceServer::new(server_service))
            .serve_with_incoming_shutdown(incoming, async {
                stop_rx.await.ok();
            })
            .await
            .unwrap();
    });
    let mut stream = ready_state_stream();
    stream.runtime.executor = GrpcEffectExecutor::for_endpoint(endpoint);
    stream
        .begin_command(ClientCommand::RetainInteractions {
            session_id: "session".into(),
        })
        .unwrap();
    stream
        .begin_command(ClientCommand::RetainInteractions {
            session_id: "session".into(),
        })
        .unwrap();
    let snapshot = wait_for_interactions(&mut stream, "initial pending snapshot", |snapshot| {
        snapshot
            .interactions
            .get("session")
            .is_some_and(|s| !s.pending.is_empty())
    })
    .await;
    let identity = snapshot.interactions["session"].pending[0].identity.clone();
    stream
        .begin_command(ClientCommand::ReleaseInteractions {
            session_id: "session".into(),
        })
        .unwrap();
    assert_eq!(
        stream.snapshot().interactions["session"].pending[0].identity,
        identity
    );
    stream
        .begin_command(ClientCommand::RespondInteraction {
            identity,
            response: crate::InteractionResponse::UrlConsent {
                action: crate::InteractionElicitationAction::Accept,
            },
        })
        .unwrap();
    wait_for_interactions(
        &mut stream,
        "authoritative response resolution",
        |snapshot| {
            snapshot
                .interactions
                .get("session")
                .is_some_and(|s| s.pending.is_empty())
        },
    )
    .await;
    assert_eq!(service.responses.lock().unwrap().len(), 1);
    stream
        .begin_command(ClientCommand::ReleaseInteractions {
            session_id: "session".into(),
        })
        .unwrap();
    assert!(stream.runtime.interaction_watches.is_empty());
    assert_eq!(service.responses.lock().unwrap().len(), 1);
    drop(stream);
    stop_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), server)
        .await
        .expect("interaction fixture server shutdown timed out")
        .unwrap();
}

async fn wait_for_interactions(
    stream: &mut ClientStateStream,
    stage: &str,
    predicate: impl Fn(&ClientSnapshot) -> bool,
) -> ClientSnapshot {
    let mut snapshots = 0;
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let snapshot = stream.next().await;
            snapshots += 1;
            if predicate(&snapshot) {
                break snapshot;
            }
            // A ready/no-op fixture sleep must not starve the transport or timeout.
            tokio::task::yield_now().await;
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("{stage} timed out after {snapshots} snapshots; state={:?}; queued={:?}; in_flight={:?}", stream.snapshot(), stream.effects, stream.in_flight.as_ref().map(|effect| &effect.effect)))
}

#[test]
fn interaction_only_interest_polls_live_watch_instead_of_idle_sleep() {
    let mut stream = ready_state_stream();
    let update = stream.runtime.reducer.retain_interactions("session");
    let [ClientEffect::Interaction(crate::InteractionEffect::Watch { epoch, .. })] =
        update.effects()
    else {
        panic!("watch");
    };
    let epoch = *epoch;
    stream
        .runtime
        .reducer
        .apply(ClientEvent::Interaction(crate::InteractionEvent::Opened {
            session_id: "session".into(),
            epoch,
        }));
    stream.runtime.interaction_watches.insert(
        "session".into(),
        (epoch, Box::pin(futures_util::stream::pending())),
    );
    stream.sleep =
        Rc::new(|_| panic!("interaction-only observation must not take the idle sleep path"));
    assert!(
        stream.next().now_or_never().is_none(),
        "an unchanged live interaction watch must remain pending"
    );
    assert!(stream.runtime.interaction_watches.contains_key("session"));
}

#[test]
fn retired_interaction_open_cannot_install_after_navigation_or_access_change() {
    let mut stream = ready_state_stream();
    stream
        .begin_command(ClientCommand::RetainInteractions {
            session_id: "session".into(),
        })
        .unwrap();
    let queued = stream.effects.pop().unwrap();
    let ClientEffect::Interaction(crate::InteractionEffect::Watch { session_id, epoch }) =
        &queued.effect
    else {
        panic!("watch");
    };
    let session_id = session_id.clone();
    let epoch = *epoch;
    stream
        .begin_command(ClientCommand::ReleaseInteractions {
            session_id: "session".into(),
        })
        .unwrap();
    stream
        .begin_command(ClientCommand::RetainInteractions {
            session_id: "session".into(),
        })
        .unwrap();
    let executed = ExecutedEffect::InteractionWatch {
        session_id: session_id.clone(),
        epoch,
        result: Ok(Box::pin(futures_util::stream::pending())),
    };
    stream.complete_effect(queued.access_generation, &queued.effect, executed);
    assert!(stream.runtime.interaction_watches.is_empty());
    let queued = stream.effects.pop().unwrap();
    let ClientEffect::Interaction(crate::InteractionEffect::Watch { epoch, .. }) = &queued.effect
    else {
        panic!("fresh watch");
    };
    let epoch = *epoch;
    stream.runtime.reducer.expire_waltr_access("expired");
    let executed = ExecutedEffect::InteractionWatch {
        session_id,
        epoch,
        result: Ok(Box::pin(futures_util::stream::pending())),
    };
    stream.complete_effect(queued.access_generation, &queued.effect, executed);
    assert!(stream.runtime.interaction_watches.is_empty());
    assert!(stream.snapshot().interactions.is_empty());
}
