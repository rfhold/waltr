use super::{PersistencePool, script_invocation::SqlInvocationStore};
use crate::routine::{
    definition::{API_VERSION, Definition},
    history::{Admission, Caller, Origin, Ownership, Revision},
    ports::InvocationStore,
};

pub(super) async fn seed(pool: PersistencePool, id: &str) -> String {
    let engine = waltr_script::Engine::new(1).unwrap();
    let definition =
        Definition::parse(include_str!("../../resources/routines/title.yaml"), &engine).unwrap();
    let store = SqlInvocationStore::new(pool);
    store
        .admit(Admission {
            id: id.into(),
            revision: Revision {
                id: definition.revision().into(),
                api_version: API_VERSION.into(),
                engine: definition.engine().into(),
                envelope: definition.canonical().into(),
                name: definition.envelope().name.clone(),
            },
            caller: Caller {
                principal: "preserved-owner".into(),
                origin: Origin::Manual,
                ownership: Ownership::Detached,
                correlation: "literal-correlation".into(),
            },
            entrypoint: "manual".into(),
            snapshot: None,
            admitted_ms: 11,
        })
        .await
        .unwrap();
    store.running(id.into(), 12).await.unwrap();
    snapshot(&store, id).await
}

pub(super) async fn snapshot(store: &SqlInvocationStore, id: &str) -> String {
    format!(
        "{:?}",
        store
            .get(id.into())
            .await
            .unwrap()
            .expect("preserved invocation")
    )
}
