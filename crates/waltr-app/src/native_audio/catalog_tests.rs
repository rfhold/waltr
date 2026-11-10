use super::*;
use crate::native_media_test_support as support;

#[test]
fn discovery_entrypoint_and_source_context_are_required_before_admission() {
    let mut s = support::snapshot("test");
    let resolve = |s: &ClientSnapshot, entrypoint: &str, source: Option<&str>| {
        Catalog::new(s).resolve(
            "test",
            entrypoint,
            source.map(str::to_owned),
            "app-test".into(),
        )
    };
    assert!(resolve(&s, "dictate", Some("draft")).is_ok());
    assert!(resolve(&s, "old-trigger", None).is_err());
    assert!(resolve(&s, "dictate", Some("unknown")).is_err());
    s.routines.definitions.definitions[0].entrypoints[0].start_contexts =
        vec![waltr_client::RoutineStartContext::Session];
    assert!(resolve(&s, "dictate", None).is_err());
    s.routines.definitions.definitions.clear();
    assert!(resolve(&s, "dictate", Some("draft")).is_err());
}

#[test]
fn resolved_parameters_come_from_the_published_schema() {
    let mut snapshot = support::snapshot("test");
    snapshot.routines.definitions.definitions[0].parameters =
        waltr_client::RoutineContract::from_wire(&waltr_client::interaction::wire::ValueContract {
            schema_version: "waltr.value/v1".into(),
            schema_json: br#"{"type":"object","properties":{"enabled":{"type":"boolean"}},"required":["enabled"]}"#.to_vec(),
        }).unwrap();
    let invocation = Catalog::new(&snapshot)
        .resolve("test", "dictate", None, "app-test".into())
        .unwrap();
    assert_eq!(
        invocation.selection.parameters.unwrap().json,
        br#"{"enabled":false}"#
    );
}

#[test]
fn pin_ignores_unrelated_updates_but_invalidates_selection_source_and_revision() {
    let s = support::snapshot("test");
    let catalog = Catalog::new(&s);
    let invocation = catalog
        .resolve("test", "dictate", Some("draft".into()), "app-test".into())
        .unwrap();
    let pin = catalog.pin(invocation, true);
    let mut unrelated = s.clone();
    unrelated.config.current.as_mut().unwrap().etag = "unrelated".into();
    unrelated
        .routines
        .definitions
        .definitions
        .push(support::definition("other"));
    assert!(Catalog::new(&unrelated).current(&pin));
    let mut changed = s.clone();
    changed
        .config
        .current
        .as_mut()
        .unwrap()
        .session_dictation
        .as_mut()
        .unwrap()
        .output = "other".into();
    assert!(!Catalog::new(&changed).current(&pin));
    let mut changed = s.clone();
    changed.sessions.current.retain(|s| s.id != "draft");
    assert!(!Catalog::new(&changed).current(&pin));
    let mut changed = s;
    changed.routines.definitions.definitions[0].routine_revision = "replacement".into();
    assert!(!Catalog::new(&changed).current(&pin));
}
