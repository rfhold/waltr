//! Shared Routine client conformance against the generated tonic service.
#[path = "support/routine.rs"]
mod fixture;

#[tokio::test]
async fn selected_old_pages_survive_recent_retention_refresh_and_expire_honestly() {
    use waltr_client::{
        ClientRoutineInvocationPage, RoutineRequest, RoutineRequestResult, RoutineRequestStatus,
    };
    let server = fixture::server().await;
    let mut stream = server
        .transport
        .watch_routine_invocations("selected".into(), None, 100, Default::default())
        .await
        .unwrap();
    let mut update = stream.next().await.unwrap().unwrap();
    let template = update.invocations[0].clone();
    update.operations.clear();
    let row = |n: u64| {
        let mut row = template.clone();
        row.invocation_id = format!("row-{n}");
        row.admission_sequence = RoutineRevision(n);
        row
    };
    let mut client = Client::default();
    update.invocations = (1000..1100).map(&row).collect();
    client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
        update.clone(),
    )));
    update.invocations = (1100..1200).map(&row).collect();
    update.cursor = update.snapshot_high_water.clone();
    update.snapshot_complete = true;
    client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
        update.clone(),
    )));
    assert_eq!(client.state().routines.invocations.len(), 200);
    for page in 0..3u64 {
        let key = format!("selected-{page}");
        client.request_routine(
            key.clone(),
            key.clone(),
            RoutineRequest::Invocations {
                after_cursor: RoutineRevision(page * 100),
                limit: 100,
                source_session: template.source_session.clone(),
            },
        );
        client.apply(ClientEvent::Routine(RoutineEvent::Requested {
            operation_key: key.clone(),
            request_id: key.clone(),
            result: RoutineRequestResult::Invocations(ClientRoutineInvocationPage {
                invocations: (page * 100 + 1..=page * 100 + 100).map(&row).collect(),
                older_page_token: Some(page * 100 + 100),
            }),
        }));
        let selected = &client.state().routines.requests[&key].selected_invocations;
        assert_eq!(selected.len(), 100);
        assert_eq!(
            selected[0].admission_sequence,
            RoutineRevision(page * 100 + 1)
        );
        assert!(
            selected
                .iter()
                .all(|r| r.source_session == template.source_session)
        );
        assert_eq!(client.state().routines.invocations.len(), 200);
        if page == 0 {
            update.phase = RoutineObservationPhase::Changes;
            update.snapshot_high_water.clear();
            update.snapshot_complete = false;
            update.cursor = "selected-change".into();
            let mut newer = row(50);
            newer.observation_revision = RoutineRevision(8);
            update.invocations = vec![newer];
            client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
                update.clone(),
            )));
            assert_eq!(
                client.state().routines.requests[&key].selected_invocations[49]
                    .observation_revision,
                RoutineRevision(8)
            );
        }
    }
    let requests = &client.state().routines.requests;
    assert_eq!(requests["selected-0"].status, RoutineRequestStatus::Expired);
    assert!(requests["selected-0"].selected_invocations.is_empty());
    assert!(requests["selected-0"].error.is_some());
    assert_eq!(
        requests
            .values()
            .map(|r| r.selected_invocations.len())
            .sum::<usize>(),
        200
    );
    client.release_routine_request("selected-1".into(), "stale-request".into());
    assert!(client.state().routines.requests.contains_key("selected-1"));
    assert!(
        client
            .release_routine_request("selected-1".into(), "selected-1".into())
            .effects()
            .is_empty()
    );
    assert!(!client.state().routines.requests.contains_key("selected-1"));
    assert_eq!(
        client
            .state()
            .routines
            .requests
            .values()
            .map(|r| r.selected_invocations.len())
            .sum::<usize>(),
        100
    );
    let late = RoutineRequestResult::Invocations(ClientRoutineInvocationPage {
        invocations: vec![row(101)],
        older_page_token: None,
    });
    client.apply(ClientEvent::Routine(RoutineEvent::Requested {
        operation_key: "selected-1".into(),
        request_id: "selected-1".into(),
        result: late,
    }));
    assert!(!client.state().routines.requests.contains_key("selected-1"));
}

#[tokio::test]
async fn unsupported_or_default_catalog_shapes_fail_closed() {
    for mode in [
        "unknown-trigger",
        "duplicate-context",
        "missing-limits",
        "missing-schema",
        "duplicate-schema-key",
        "unknown-schema",
    ] {
        let server = fixture::server().await;
        server.service.catalog_mode(mode);
        let mut watch = server
            .transport
            .watch_manual_routines("catalog".into())
            .await
            .unwrap();
        assert!(watch.next().await.unwrap().is_err(), "{mode}");
    }
    assert!(serde_json::from_str::<waltr_client::RoutineJson>(r#""{\"x\":1,\"x\":2}""#).is_err());
    assert!(serde_json::from_str::<waltr_client::RoutineRevision>("9007199254740993").is_err());
}

#[tokio::test]
async fn keyed_paged_reads_and_cancel_receipts_do_not_fabricate_history() {
    use waltr_client::{RoutineRequest, RoutineRequestStatus};
    let server = fixture::server().await;
    let mut client = Client::default();
    let command = RoutineRequest::Invocations {
        after_cursor: RoutineRevision(0),
        limit: 50,
        source_session: Some("session".into()),
    };
    client.request_routine("page".into(), "old".into(), command.clone());
    client.request_routine("page".into(), "current".into(), command.clone());
    let result = server.transport.request_routine(command).await.unwrap();
    client.apply(ClientEvent::Routine(RoutineEvent::Requested {
        operation_key: "page".into(),
        request_id: "old".into(),
        result: result.clone(),
    }));
    assert!(client.state().routines.invocations.is_empty());
    client.apply(ClientEvent::Routine(RoutineEvent::Requested {
        operation_key: "page".into(),
        request_id: "current".into(),
        result,
    }));
    let before = client.state().routines.invocations.clone();
    let cancel = RoutineRequest::Cancel {
        invocation_id: "invocation".into(),
    };
    client.request_routine("cancel".into(), "cancel".into(), cancel.clone());
    let result = server.transport.request_routine(cancel).await.unwrap();
    client.apply(ClientEvent::Routine(RoutineEvent::Requested {
        operation_key: "cancel".into(),
        request_id: "cancel".into(),
        result,
    }));
    assert_eq!(client.state().routines.invocations, before);
    assert_eq!(
        client.state().routines.requests["cancel"].status,
        RoutineRequestStatus::Ready
    );
}

#[tokio::test]
async fn observation_retention_preserves_detail_and_start_pins_but_bounds_unpinned_rows() {
    let server = fixture::server().await;
    let mut stream = server
        .transport
        .watch_routine_invocations("history".into(), None, 50, Default::default())
        .await
        .unwrap();
    let baseline = stream.next().await.unwrap().unwrap();
    let template = baseline.invocations[0].clone();
    let mut client = Client::default();
    client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
        baseline.clone(),
    )));
    let mut done = baseline.clone();
    done.invocations.clear();
    done.cursor = "opaque/high-water".into();
    done.snapshot_complete = true;
    client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
        done,
    )));
    client.apply(ClientEvent::Routine(RoutineEvent::DetailRequested {
        request_id: "pinned".into(),
        invocation_id: "invocation".into(),
    }));
    for page in 0..4u64 {
        let mut update = baseline.clone();
        update.phase = RoutineObservationPhase::Changes;
        update.snapshot_high_water.clear();
        update.snapshot_complete = false;
        update.cursor = format!("changes-{page}");
        update.invocations = (0..100u64)
            .map(|n| {
                let mut row = template.clone();
                row.invocation_id = format!("row-{}", page * 100 + n);
                row.admission_sequence = RoutineRevision(page * 100 + n + 1);
                row
            })
            .collect();
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            update,
        )));
    }
    assert_eq!(client.state().routines.invocations.len(), 201);
    assert!(
        client
            .state()
            .routines
            .invocations
            .contains_key("invocation")
    );
    assert!(client.state().routines.invocations.contains_key("row-399"));
    assert!(!client.state().routines.invocations.contains_key("row-0"));
}

use futures_util::StreamExt;
use waltr_client::{
    Client, ClientEvent, RoutineEvent, RoutineObservationPhase, RoutineRevision, WaltrTransport,
};

#[tokio::test]
async fn actual_routine_catalog_history_operations_and_atomic_detail() {
    let server = fixture::server().await;
    let mut definitions = server
        .transport
        .watch_manual_routines("definitions".into())
        .await
        .unwrap();
    let catalog = definitions.next().await.unwrap().unwrap();
    assert_eq!(catalog.definitions[0].entrypoints.len(), 2);
    assert!(catalog.definitions[0].permits("manual", false, Some("session")));
    assert!(!catalog.definitions[0].permits("live", false, None));
    let mut client = Client::default();
    client.apply(ClientEvent::Routine(RoutineEvent::DefinitionWatchChanged(
        Box::new(catalog),
    )));
    let mut history = server
        .transport
        .watch_routine_invocations("history".into(), None, 50, Default::default())
        .await
        .unwrap();
    for phase in [
        RoutineObservationPhase::Snapshot,
        RoutineObservationPhase::Snapshot,
        RoutineObservationPhase::Changes,
    ] {
        let update = history.next().await.unwrap().unwrap();
        assert_eq!(update.phase, phase);
        client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
            update,
        )));
    }
    assert_eq!(
        client.state().routines.through_sequence.as_deref(),
        Some("not-a-number/change")
    );
    assert_eq!(
        client.state().routines.operations["operation"].observation_revision,
        RoutineRevision(4)
    );
    assert_eq!(
        client.state().routines.invocations["invocation"].observation_revision,
        RoutineRevision(3)
    );
    let json = serde_json::to_string(&client.state().routines.invocations["invocation"]).unwrap();
    assert!(json.contains(&format!("\"admissionSequence\":\"{}\"", u64::MAX - 1)));
    assert!(!json.contains("tasks"));
    let selected = waltr_client::RoutineRequest::Invocations {
        after_cursor: RoutineRevision(0),
        limit: 1,
        source_session: None,
    };
    client.request_routine(
        "get-selected".into(),
        "get-selected".into(),
        selected.clone(),
    );
    let result = server.transport.request_routine(selected).await.unwrap();
    client.apply(ClientEvent::Routine(RoutineEvent::Requested {
        operation_key: "get-selected".into(),
        request_id: "get-selected".into(),
        result,
    }));
    let detail = server
        .transport
        .get_routine_invocation("detail".into(), "invocation".into())
        .await
        .unwrap();
    client.apply(ClientEvent::Routine(RoutineEvent::DetailRequested {
        request_id: "detail".into(),
        invocation_id: "invocation".into(),
    }));
    client.apply(ClientEvent::Routine(RoutineEvent::DetailLoaded {
        request_id: "detail".into(),
        invocation_id: "invocation".into(),
        detail: Box::new(detail),
    }));
    assert_eq!(
        client.state().routines.invocations["invocation"].observation_revision,
        RoutineRevision(9)
    );
    assert_eq!(
        client.state().routines.requests["get-selected"].selected_invocations[0]
            .observation_revision,
        RoutineRevision(9)
    );
    // A delayed journal upsert cannot roll back a newer atomic Get.
    let mut resumed = server
        .transport
        .watch_routine_invocations(
            "resume".into(),
            Some("not-a-number/change".into()),
            50,
            Default::default(),
        )
        .await
        .unwrap();
    client.apply(ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(
        resumed.next().await.unwrap().unwrap(),
    )));
    assert_eq!(
        client.state().routines.invocations["invocation"].observation_revision,
        RoutineRevision(9)
    );
    assert_eq!(
        client.state().routines.requests["get-selected"].selected_invocations[0]
            .observation_revision,
        RoutineRevision(9)
    );
    assert_eq!(
        client.state().routines.details["invocation"]
            .detail
            .as_ref()
            .unwrap()
            .pinned_definition
            .routine_revision,
        "revision"
    );
}

#[tokio::test]
async fn source_filters_opaque_resume_and_explicit_expiry_reset() {
    let server = fixture::server().await;
    let filter = std::collections::BTreeMap::from([(
        waltr_client::ROUTINE_SOURCE_SESSION_ID_METADATA_KEY.into(),
        "session".into(),
    )]);
    let mut history = server
        .transport
        .watch_routine_invocations(
            "filtered".into(),
            Some("partial/snapshot".into()),
            50,
            filter,
        )
        .await
        .unwrap();
    history.next().await.unwrap().unwrap();
    let requests = server.service.requests();
    assert_eq!(requests[0].source_session.as_deref(), Some("session"));
    assert_eq!(requests[0].after_cursor, "partial/snapshot");
    let mut expired = server
        .transport
        .watch_routine_invocations(
            "expired".into(),
            Some("expired".into()),
            50,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        expired.next().await.unwrap().unwrap().phase,
        RoutineObservationPhase::ResetRequired
    );
    assert_eq!(server.service.admissions(), (0, 0));
}

#[tokio::test]
async fn starts_are_current_catalog_gated_and_completion_is_not_collection_authority() {
    let server = fixture::server().await;
    let mut catalog = server
        .transport
        .watch_manual_routines("catalog".into())
        .await
        .unwrap();
    let mut client = Client::default();
    client.apply(ClientEvent::Routine(RoutineEvent::DefinitionWatchChanged(
        Box::new(catalog.next().await.unwrap().unwrap()),
    )));
    let invalid = client.start_manual_routine(
        "invalid".into(),
        "r",
        "routine",
        "stale",
        "manual",
        None,
        waltr_client::RoutineJson::parse("{}").unwrap(),
    );
    assert!(invalid.effects().is_empty());
    let update = client.start_manual_routine(
        "start".into(),
        "correlation",
        "routine",
        "revision",
        "manual",
        Some("session".into()),
        waltr_client::RoutineJson::parse("{}").unwrap(),
    );
    assert_eq!(update.effects().len(), 1);
    let result = server
        .transport
        .start_manual_routine(waltr_client::StartManualRoutineArgs {
            request_id: "correlation".into(),
            routine_name: "routine".into(),
            routine_revision: "revision".into(),
            trigger_name: "manual".into(),
            source_session_id: Some("session".into()),
            parameters: waltr_client::RoutineJson::parse("{}").unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        result,
        waltr_client::ClientRoutineStartOutcome::Accepted("invocation".into())
    );
    client.apply(ClientEvent::Routine(RoutineEvent::StartAccepted {
        operation_key: "start".into(),
        request_id: "correlation".into(),
        invocation_id: "invocation".into(),
    }));
    assert!(client.state().routines.invocations.is_empty());
}
