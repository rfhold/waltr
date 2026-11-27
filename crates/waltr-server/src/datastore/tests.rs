use super::*;

#[test]
fn datastore_storage_is_disjoint_from_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    assert!(Storage::validate_workspace_path(&workspace, &root.path().join("objects")).is_ok());
    assert_eq!(Storage::validate_workspace_path(&workspace, &workspace.join("data")), Err(Error::Invalid));
    assert_eq!(Storage::validate_workspace_path(&workspace, root.path()), Err(Error::Invalid));
    #[cfg(unix)]
    {
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&workspace, &alias).unwrap();
        assert_eq!(Storage::validate_workspace_path(&workspace, &alias.join("data")), Err(Error::Invalid));
    }
}

#[tokio::test]
async fn datastore_deactivation_waits_for_admitted_backend_work() {
    let (root, data) = fixture().await;
    let admitted = data.catalog.fence_data_store_commit().await;
    std::fs::remove_file(root.path().join(".waltr/resources/data-stores/notes.yaml")).unwrap();
    let reload = data.catalog.reload_from_disk();
    tokio::pin!(reload);
    assert!(futures_util::poll!(&mut reload).is_pending());
    assert!(data.catalog.snapshot().data_store("notes").is_some());
    drop(admitted);
    reload.await;
    assert!(data.catalog.snapshot().data_store("notes").is_none());
}

async fn fixture() -> (tempfile::TempDir, Arc<DataStores>) {
    let root = tempfile::tempdir().unwrap();
    let resources = root.path().join(".waltr/resources");
    for (subdir, name, source) in [
        (
            "mcp-servers",
            "waltr",
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {name: waltr}\nspec: {transport: streamableHttp, url: 'https://waltr.example', path: /mcp}\n",
        ),
        (
            "data-stores",
            "files",
            "apiVersion: waltr.dev/v1alpha1\nkind: DataStore\nmetadata: {name: files}\nspec: {type: object, access: {scope: principal}}\n",
        ),
        (
            "data-stores",
            "notes",
            "apiVersion: waltr.dev/v1alpha1\nkind: DataStore\nmetadata: {name: notes}\nspec:\n  type: relational\n  access: {scope: shared}\n  collections:\n    note:\n      fields: {title: {type: string, required: true}}\n",
        ),
    ] {
        std::fs::create_dir_all(resources.join(subdir)).unwrap();
        std::fs::write(resources.join(subdir).join(format!("{name}.yaml")), source).unwrap();
    }
    let materializer =
        crate::catalog::CatalogMaterializer::load(&crate::test_support::default_catalog_loader())
            .await
            .unwrap();
    let state = AgentDefinitionState::load_unified(root.path(), materializer).unwrap();
    let data = Storage::local(root.path()).await.unwrap().bind(state);
    (root, data)
}

#[tokio::test]
async fn datastore_principal_scope_and_transfer_authority() {
    let (_root, data) = fixture().await;
    let alice = AuthenticatedPrincipal::apps_test_principal("alice");
    let bob = AuthenticatedPrincipal::apps_test_principal("bob");
    let anon = AuthenticatedPrincipal::anonymous_disabled();
    assert_eq!(
        data.read(&anon, ROOT, None).await.unwrap()["stores"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let file = "dir/content";
    let upload = data
        .call(
            &alice,
            "create",
            "object.upload",
            json!({"store":"files","key":file,"size":3}),
            None,
        )
        .await
        .unwrap();
    let transfer = upload["transfer"].as_str().unwrap();
    let chunk = json!({"store":"files","transfer":transfer,"offset":0,"content_base64":"AAH/"});
    assert_eq!(
        data.call(&bob, "execute", "object.upload_chunk", chunk.clone(), None)
            .await,
        Err(Error::NotFound)
    );
    data.call(&alice, "execute", "object.upload_chunk", chunk, None)
        .await
        .unwrap();
    let write = json!({"store":"files","key":file,"transfer":transfer});
    let created = data
        .call(&alice, "create", "object.create", write.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        data.call(&alice, "create", "object.create", write, None)
            .await,
        Err(Error::NotFound)
    );
    let uri = created["content_uri"].as_str().unwrap();
    assert_eq!(data.read(&bob, uri, None).await, Err(Error::NotFound));
    assert_eq!(
        data.read(&alice, uri, None).await.unwrap()["content_base64"],
        "AAH/"
    );
}

#[tokio::test]
async fn datastore_shared_attribution_and_transaction_rollback() {
    let (_root, data) = fixture().await;
    let alice = AuthenticatedPrincipal::apps_test_principal("alice");
    let bob = AuthenticatedPrincipal::apps_test_principal("bob");
    let tx = data.begin();
    let notes = json!({"store":"notes","collection":"note","id":"n1","fields":{"title":"First"}});
    data.call(&alice, "create", "record.create", notes.clone(), Some(&tx))
        .await
        .unwrap();
    let uri = record_uri("notes", "note", "n1");
    assert_eq!(
        data.read(&alice, &uri, Some(&tx)).await.unwrap()["fields"]["title"],
        "First"
    );
    assert_eq!(
        data.call(&alice, "create", "record.create", notes.clone(), Some(&tx))
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(tx.finish("first", true).await, Err(Error::Conflict));
    assert_eq!(data.read(&bob, &uri, None).await, Err(Error::NotFound));

    let record = data
        .call(&alice, "create", "record.create", notes, None)
        .await
        .unwrap();
    let editor = json!({"store":"notes","collection":"note","id":"n1","fields":{"title":"Edited"}});
    let updated = data
        .call(&bob, "execute", "record.update", editor, None)
        .await
        .unwrap();
    assert_eq!(
        record["attribution"]["created_by"],
        updated["attribution"]["created_by"]
    );
    assert_ne!(
        updated["attribution"]["created_by"],
        updated["attribution"]["last_edited_by"]
    );
}

#[tokio::test]
async fn datastore_restart_reconciles_commit_receipt_without_replay() {
    use crate::persistence::{DatabaseConfig, Persistence};
    use crate::routine::{
        definition::{API_VERSION, revision_id},
        history::{Admission, Caller, Operation, OperationStatus, Origin, Ownership, Revision, Status},
        ports::InvocationStore,
    };
    let (root, data) = fixture().await;
    let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
        .await
        .unwrap();
    let journal = persistence.script_invocation_store();
    let engine = "test-js;test-engine;abi=1";
    let envelope = r#"{"apiVersion":"waltr.routine/v1","kind":"Routine","name":"test","spec":{"source":"export default async function() {}"}}"#;
    let alice = AuthenticatedPrincipal::apps_test_principal("alice");
    for (id, committed) in [("recovered", true), ("unconfirmed", false)] {
        journal.admit(Admission {
            id: id.into(),
            revision: Revision {
                id: revision_id(engine, envelope),
                api_version: API_VERSION.into(),
                engine: engine.into(),
                envelope: envelope.into(),
                name: "test".into(),
            },
            caller: Caller {
                principal: "alice".into(), origin: Origin::Manual,
                ownership: Ownership::Detached, correlation: id.into(), origin_machine_id: None,
            },
            entrypoint: "run".into(), snapshot: None, admitted_ms: 1,
        }).await.unwrap();
        journal.running(id.into(), 2).await.unwrap();
        journal.dispatch_intent(Operation {
            version: 1, cursor: 0, id: format!("marker-{id}"), invocation: id.into(),
            name: "datastore.commit".into(), status: OperationStatus::Dispatched,
            arguments: json!({}), result: None, updated_ms: 3,
        }).await.unwrap();
        if committed {
            let tx = data.begin();
            data.call(&alice, "create", "record.create",
                json!({"store":"notes","collection":"note","id":id,"fields":{"title":"Saved"}}),
                Some(&tx)).await.unwrap();
            tx.finish(id, true).await.unwrap();
        }
    }
    let state = data.catalog.clone();
    drop(data);
    let reopened = Storage::local(root.path()).await.unwrap().bind(state);
    journal.reconcile_datastore_commits(&reopened, 4).await.unwrap();
    assert_eq!(journal.reconcile(5).await.unwrap(), 0);
    for (id, expected) in [("recovered", OperationStatus::Completed), ("unconfirmed", OperationStatus::Uncertain)] {
        assert_eq!(journal.get(id.into()).await.unwrap().unwrap().status, Status::Uncertain);
        let ops = journal.operations(id.into(), 0, 10).await.unwrap();
        assert_eq!(ops[0].status, expected);
        assert_eq!(ops[0].result, (id == "recovered").then(|| json!({"commit":"confirmed"})));
    }
    let result = reopened.call(&alice, "query", "record.find",
        json!({"store":"notes","collection":"note"}), None).await.unwrap();
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn datastore_catalog_does_not_reinterpret_data_after_restart() {
    let (root, original) = fixture().await;
    original.install_catalog_validation().await.unwrap();
    drop(original);
    let path = root.path().join(".waltr/resources/data-stores/notes.yaml");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, source.replace("type: string", "type: integer")).unwrap();

    let materializer =
        crate::catalog::CatalogMaterializer::load(&crate::test_support::default_catalog_loader())
            .await
            .unwrap();
    let changed = AgentDefinitionState::load_unified(root.path(), materializer.clone()).unwrap();
    let changed = Storage::local(root.path()).await.unwrap().bind(changed);
    assert!(changed.install_catalog_validation().await.is_err());
    drop(changed);

    std::fs::write(&path, source).unwrap();
    let restored = AgentDefinitionState::load_unified(root.path(), materializer).unwrap();
    let restored = Storage::local(root.path()).await.unwrap().bind(restored);
    restored.install_catalog_validation().await.unwrap();
}

#[tokio::test]
async fn datastore_reattachment_preserves_external_data_and_rejects_old_transactions() {
    let (root, data) = fixture().await;
    let alice = AuthenticatedPrincipal::apps_test_principal("alice");
    let tx = data.begin();
    let query = json!({"store":"notes","collection":"note"});
    data.call(&alice, "query", "record.find", query.clone(), Some(&tx))
        .await
        .unwrap();

    let path = root.path().join(".waltr/resources/data-stores/notes.yaml");
    let source = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    data.catalog.reload_from_disk().await;
    assert_eq!(
        data.call(&alice, "query", "record.find", query.clone(), None)
            .await,
        Err(Error::NotFound)
    );
    std::fs::write(&path, source).unwrap();
    data.catalog.reload_from_disk().await;
    assert_eq!(
        tx.finish("old-activation", true).await,
        Err(Error::Conflict)
    );

    let connection = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(root.path().join("datastore.sqlite3")),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO records(namespace, collection, id, fields) VALUES ('relational/notes/shared', 'note', 'external', '{\"title\":\"Outside\"}')")
        .execute(&connection).await.unwrap();
    let result = data
        .call(&alice, "query", "record.find", query, None)
        .await
        .unwrap();
    assert_eq!(result["items"][0]["fields"]["title"], "Outside");
    assert_eq!(
        result["items"][0]["attribution"]["last_edited_by"],
        Value::Null
    );
}
