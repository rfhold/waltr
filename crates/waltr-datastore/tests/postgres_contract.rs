//! Shared relational contract for SQLite and disposable PostgreSQL.

use std::path::Path;

use serde_json::json;
use sqlx::{Connection, PgConnection, SqliteConnection, sqlite::SqliteConnectOptions};
use waltr_datastore::{
    Actor, Attribution, Cardinality, Definition, Error, FieldType, Namespace, Scope,
    relational::{Direction, Fields, Filter, FilterOp, Query, Relational, Sort, Traversal},
};

enum Stock {
    Sqlite(SqliteConnection),
    Postgres(PgConnection),
}

impl Stock {
    async fn sqlite(path: &Path) -> Self {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(false)
            .pragma("recursive_triggers", "OFF");
        Self::Sqlite(SqliteConnection::connect_with(&options).await.unwrap())
    }

    async fn postgres(url: &str) -> Self {
        let mut conn = PgConnection::connect(url).await.unwrap();
        sqlx::query("SET search_path = waltr_datastore")
            .execute(&mut conn)
            .await
            .unwrap();
        Self::Postgres(conn)
    }
}

macro_rules! raw {
    ($stock:expr, $sqlite:literal, $pg:literal $(, $bind:expr)* $(,)?) => {
        match $stock {
            Stock::Sqlite(conn) => sqlx::query($sqlite)$(.bind($bind))*.execute(conn).await.map(|result| result.rows_affected()),
            Stock::Postgres(conn) => sqlx::query($pg)$(.bind($bind))*.execute(conn).await.map(|result| result.rows_affected()),
        }
    };
    ($stock:expr, $both:literal $(, $bind:expr)* $(,)?) => {
        raw!($stock, $both, $both $(, $bind)*)
    };
}

fn fields(value: serde_json::Value) -> Fields {
    value.as_object().unwrap().clone()
}

fn definition() -> Definition {
    let definition: Definition = serde_json::from_value(json!({
        "type": "relational",
        "access": {"scope": "shared"},
        "collections": {
            "notes": {"fields": {
                "title": {"type": "string", "required": true},
                "order": {"type": "integer", "indexed": true},
                "measurement": {"type": "number", "indexed": true},
                "flag": {"type": "boolean", "indexed": true},
                "extra": {"type": "json"}
            }},
            "tags": {"fields": {"text": {"type": "string", "required": true}}}
        },
        "relations": {
            "tag": {"from": "notes", "to": "tags", "cardinality": "one-to-many"},
            "primary": {"from": "notes", "to": "tags", "cardinality": "one-to-one"},
            "related": {"from": "notes", "to": "tags", "cardinality": "many-to-many"}
        }
    }))
    .unwrap();
    definition.validate().unwrap();
    definition
}

#[tokio::test]
async fn sqlite_contract() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.sqlite3");
    let db = Relational::open_sqlite(&path).await.unwrap();
    let mut stock = Stock::sqlite(&path).await;
    raw_first_links(&db, &mut stock).await;
    let definitions = register_definitions(&db, &mut stock).await;
    let receipt = contract(&db, &mut stock).await;
    drop(db);
    let reopened = Relational::open_sqlite(&path).await.unwrap();
    assert!(reopened.has_receipt(&receipt).await.unwrap());
    verify_retained_definitions(&reopened, &definitions).await;
}

#[tokio::test]
async fn sqlite_incompatible_migration_is_non_destructive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid.sqlite3");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("CREATE TABLE records (old_data TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO records VALUES ('preserved')")
        .execute(&mut conn)
        .await
        .unwrap();
    assert!(matches!(
        Relational::open_sqlite(&path).await,
        Err(Error::Unavailable)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT old_data FROM records")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        "preserved",
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_schema WHERE name = 'links'")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        0,
    );
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database (WALTR_DATASTORE_TEST_POSTGRES_URL)"]
async fn postgres_contract() {
    let url = std::env::var("WALTR_DATASTORE_TEST_POSTGRES_URL")
        .expect("set WALTR_DATASTORE_TEST_POSTGRES_URL to a disposable PostgreSQL database");
    let db = Relational::open_postgres(&url).await.unwrap();
    let mut stock = Stock::postgres(&url).await;
    raw_first_links(&db, &mut stock).await;
    let definitions = register_definitions(&db, &mut stock).await;
    let receipt = contract(&db, &mut stock).await;
    drop(db);
    let reopened = Relational::open_postgres(&url).await.unwrap();
    assert!(reopened.has_receipt(&receipt).await.unwrap());
    verify_retained_definitions(&reopened, &definitions).await;
}

async fn raw_first_links(db: &Relational, stock: &mut Stock) {
    macro_rules! add_link {
        ($($value:expr),* $(,)?) => {
            raw!(
                &mut *stock,
                "INSERT INTO links
                 (namespace, relation, from_collection, from_id, to_collection, to_id, cardinality)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                $($value),*
            )
        };
    }
    let shared_name = uuid::Uuid::new_v4().to_string();
    let private_name = uuid::Uuid::new_v4().to_string();
    let shared = definition();
    let mut private = shared.clone();
    private.access.scope = Scope::Principal;
    let alice = Actor::from_trusted_key("alice".into()).unwrap();
    let bob = Actor::from_trusted_key("bob".into()).unwrap();
    let shared_partition = Namespace::resolve(&shared_name, &shared, None).unwrap();
    let alice_partition = Namespace::resolve(&private_name, &private, Some(&alice)).unwrap();
    let bob_partition = Namespace::resolve(&private_name, &private, Some(&bob)).unwrap();
    db.ensure_definitions([(&*shared_name, &shared), (&*private_name, &private)])
        .await
        .unwrap();

    let wrong_scope = format!("relational/{private_name}/shared");
    let bad_hash = format!("relational/{private_name}/principal-{}", "g".repeat(64));
    let partitions = [
        (
            shared_partition.as_str(),
            Some((&shared_partition, &shared)),
        ),
        (alice_partition.as_str(), Some((&alice_partition, &private))),
        (bob_partition.as_str(), Some((&bob_partition, &private))),
        (&wrong_scope, None),
        (&bad_hash, None),
    ];
    raw!(
        &mut *stock,
        "INSERT INTO records (namespace, collection, id, fields)
         VALUES ($1, 'tags', 'bob-only', '{\"text\":\"bob\"}')",
        bob_partition.as_str(),
    )
    .unwrap();

    for (partition, canonical) in partitions {
        raw!(
            &mut *stock,
            "INSERT INTO records (namespace, collection, id, fields) VALUES
            ($1, 'notes', 'first', '{\"title\":\"one\"}'),
            ($1, 'notes', 'second', '{\"title\":\"two\"}'),
            ($1, 'tags', 'tag', '{\"text\":\"tag\"}'),
            ($1, 'tags', 'spare', '{\"text\":\"spare\"}')",
            partition,
        )
        .unwrap();
        for (relation, source, from, target, to, cardinality) in [
            ("unknown", "notes", "first", "tags", "tag", "one-to-one"),
            ("primary", "notes", "first", "tags", "tag", "many-to-many"),
            ("primary", "tags", "tag", "notes", "first", "one-to-one"),
        ] {
            assert!(add_link!(partition, relation, source, from, target, to, cardinality).is_err());
        }
        if partition == alice_partition.as_str() {
            assert!(
                add_link!(
                    partition,
                    "primary",
                    "notes",
                    "first",
                    "tags",
                    "bob-only",
                    "one-to-one"
                )
                .is_err()
            );
        }
        let first_link = add_link!(
            partition,
            "primary",
            "notes",
            "first",
            "tags",
            "tag",
            "one-to-one",
        );
        let Some((namespace, def)) = canonical else {
            assert!(first_link.is_err());
            continue;
        };
        assert_eq!(first_link.unwrap(), 1);
        for (from, to) in [("first", "tag"), ("first", "spare"), ("second", "tag")] {
            assert!(
                add_link!(
                    partition,
                    "primary",
                    "notes",
                    from,
                    "tags",
                    to,
                    "one-to-one"
                )
                .is_err()
            );
        }
        let page = db
            .traverse(namespace, def, "primary", "first", &Traversal::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].record.id, "tag");
        assert_eq!(
            page.items[0].link.attribution,
            Attribution {
                created_by: None,
                last_edited_by: None,
            },
        );
    }
}

async fn register_definitions(db: &Relational, stock: &mut Stock) -> String {
    let name = uuid::Uuid::new_v4().to_string();
    let initial = definition();
    let object: Definition =
        serde_json::from_value(json!({"type": "object", "access": {"scope": "principal"}}))
            .unwrap();
    let namespace = Namespace::resolve(&name, &initial, None).unwrap();
    raw!(
        &mut *stock,
        "INSERT INTO records (namespace, collection, id, fields) VALUES ($1, 'notes', 'raw', '{\"title\":\"raw\"}')",
        namespace.as_str(),
    )
    .unwrap();
    let object_name = format!("{name}-object");
    db.ensure_definitions([(&*name, &initial), (&*object_name, &object)])
        .await
        .unwrap();
    assert_eq!(
        db.get(&namespace, &initial, "notes", "raw")
            .await
            .unwrap()
            .fields,
        fields(json!({"title": "raw"}))
    );

    let unregistered = format!("0-{name}");
    let mut other = initial.clone();
    other.access.scope = Scope::Principal;
    assert_eq!(
        db.ensure_definitions([(&*unregistered, &initial), (&*name, &other)])
            .await,
        Err(Error::Conflict)
    );
    db.check_definitions([(&*unregistered, &initial)])
        .await
        .unwrap();
    db.ensure_definitions([(&*unregistered, &other)])
        .await
        .unwrap();

    let competing = format!("{name}-competing");
    let (left, right) = tokio::join!(
        db.ensure_definitions([(&*competing, &initial)]),
        db.ensure_definitions([(&*competing, &other)]),
    );
    assert!(
        (left == Ok(()) && right == Err(Error::Conflict))
            || (right == Ok(()) && left == Err(Error::Conflict))
    );
    name
}

async fn verify_retained_definitions(db: &Relational, name: &str) {
    let initial = definition();
    db.ensure_definitions([]).await.unwrap();
    let mut scope = initial.clone();
    scope.access.scope = Scope::Principal;
    let kind: Definition =
        serde_json::from_value(json!({"type": "object", "access": {"scope": "shared"}})).unwrap();
    let mut indexed = initial.clone();
    indexed
        .collections
        .get_mut("notes")
        .unwrap()
        .fields
        .get_mut("order")
        .unwrap()
        .indexed = false;
    let mut field_type = initial.clone();
    field_type
        .collections
        .get_mut("notes")
        .unwrap()
        .fields
        .get_mut("order")
        .unwrap()
        .kind = FieldType::Number;
    let mut field_addition = initial.clone();
    let specs = &mut field_addition.collections.get_mut("notes").unwrap().fields;
    specs.insert("addition".into(), specs["order"].clone());
    let mut cardinality = initial.clone();
    cardinality
        .relations
        .get_mut("primary")
        .unwrap()
        .cardinality = Cardinality::OneToMany;
    for candidate in [
        &scope,
        &kind,
        &indexed,
        &field_type,
        &field_addition,
        &cardinality,
    ] {
        assert_eq!(
            db.check_definitions([(name, candidate)]).await,
            Err(Error::Conflict),
        );
        assert_eq!(
            db.ensure_definitions([(name, candidate)]).await,
            Err(Error::Conflict),
        );
    }
    assert_eq!(
        db.ensure_definitions([(&*format!("{name}-object"), &initial)])
            .await,
        Err(Error::Conflict)
    );
    db.ensure_definitions([(name, &initial)]).await.unwrap();
    let namespace = Namespace::resolve(name, &initial, None).unwrap();
    assert_eq!(
        db.get(&namespace, &initial, "notes", "raw")
            .await
            .unwrap()
            .fields,
        fields(json!({"title": "raw"})),
    );
}

async fn contract(db: &Relational, stock: &mut Stock) -> String {
    let store_name = uuid::Uuid::new_v4().to_string();
    let definition = definition();
    let namespace = Namespace::resolve(&store_name, &definition, None).unwrap();
    let part = namespace.as_str();
    let alice = Actor::from_trusted_key("alice".into()).unwrap();
    let bob = Actor::from_trusted_key("bob".into()).unwrap();
    let alice_only = Attribution {
        created_by: Some("alice".into()),
        last_edited_by: Some("alice".into()),
    };

    assert!(
        db.find(&namespace, &definition, "notes", &Query::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let note = fields(json!({"title": "made by Alice"}));
    let created = db
        .create(&namespace, &definition, &alice, "notes", "missing", &note)
        .await
        .unwrap();
    assert_eq!(
        (created.fields, created.attribution),
        (note, alice_only.clone())
    );
    let updated = db
        .update(
            &namespace,
            &definition,
            &bob,
            "notes",
            "missing",
            &fields(json!({"title": "edited by Bob"})),
        )
        .await
        .unwrap();
    assert_eq!(updated.attribution.created_by.as_deref(), Some("alice"));
    assert_eq!(updated.attribution.last_edited_by.as_deref(), Some("bob"));

    raw!(
        &mut *stock,
        "UPDATE records SET fields = fields WHERE namespace = $1 AND id = 'missing'",
        part,
    )
    .unwrap();
    assert_eq!(
        db.get(&namespace, &definition, "notes", "missing")
            .await
            .unwrap()
            .attribution,
        Attribution {
            created_by: Some("alice".into()),
            last_edited_by: None,
        },
    );

    db.update(
        &namespace,
        &definition,
        &bob,
        "notes",
        "missing",
        &updated.fields,
    )
    .await
    .unwrap();

    raw!(
        &mut *stock,
        "UPDATE records SET fields = $2 WHERE namespace = $1 AND id = 'missing'",
        "UPDATE records SET fields = CAST($2 AS JSONB) WHERE namespace = $1 AND id = 'missing'",
        part,
        r#"{"title":"outside"}"#,
    )
    .unwrap();
    let observed = db
        .get(&namespace, &definition, "notes", "missing")
        .await
        .unwrap();
    assert_eq!(observed.fields, fields(json!({"title": "outside"})));
    assert_eq!(observed.attribution.created_by.as_deref(), Some("alice"));
    assert_eq!(observed.attribution.last_edited_by, None);

    for (id, data) in [
        ("null", r#"{"title":"raw","order":null}"#),
        (
            "b",
            r#"{"title":"raw","order":9007199254740993,"measurement":0.12345678912345678,"flag":true}"#,
        ),
        (
            "a",
            r#"{"title":"raw","order":9007199254740993,"measurement":0.12345678912345678,"flag":false}"#,
        ),
        (
            "c",
            r#"{"title":"raw","order":9007199254740992,"measurement":0.12345678912345676}"#,
        ),
    ] {
        raw!(
            &mut *stock,
            "INSERT INTO records VALUES ($1, 'notes', $2, $3, 'forged', 'forged')",
            "INSERT INTO records VALUES ($1, 'notes', $2, CAST($3 AS JSONB), 'forged', 'forged')",
            part,
            id,
            data,
        )
        .unwrap();
        assert_eq!(
            db.get(&namespace, &definition, "notes", id)
                .await
                .unwrap()
                .attribution,
            Attribution::default()
        );
    }

    for (op, value, ids) in [
        (FilterOp::IsNull, None, vec!["null"]),
        (FilterOp::IsMissing, None, vec!["missing"]),
        (
            FilterOp::Gt,
            Some(json!(9007199254740992_i64)),
            vec!["a", "b"],
        ),
    ] {
        let query = Query {
            filters: vec![Filter {
                field: "order".into(),
                op,
                value,
            }],
            ..Query::default()
        };
        let found = db
            .find(&namespace, &definition, "notes", &query)
            .await
            .unwrap();
        assert_eq!(
            found
                .items
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ids
        );
    }

    for (field, descending, expected) in [
        ("order", false, vec!["missing", "null", "c", "a", "b"]),
        ("order", true, vec!["b", "a", "c", "null", "missing"]),
        ("measurement", false, vec!["missing", "null", "c", "a", "b"]),
    ] {
        let mut query = Query {
            sort: Some(Sort {
                field: field.into(),
                descending,
            }),
            limit: 1,
            ..Query::default()
        };
        for (index, id) in expected.iter().enumerate() {
            let page = db
                .find(&namespace, &definition, "notes", &query)
                .await
                .unwrap();
            assert_eq!(page.items.len(), 1);
            assert_eq!(&page.items[0].id, id);
            if index == 0 {
                let another = Namespace::resolve("no-cursor-crossing", &definition, None).unwrap();
                let wrong = Query {
                    cursor: page.cursor.clone(),
                    ..query.clone()
                };
                assert_eq!(
                    db.find(&another, &definition, "notes", &wrong).await,
                    Err(Error::Invalid)
                );
            }
            assert_eq!(page.cursor.is_some(), index + 1 < expected.len());
            query.cursor = page.cursor;
        }
    }

    let tag = fields(json!({"text": "tag"}));
    for id in ["one", "two"] {
        db.create(&namespace, &definition, &alice, "tags", id, &tag)
            .await
            .unwrap();
    }
    let mut principal = definition.clone();
    principal.access.scope = waltr_datastore::Scope::Principal;
    let private = Namespace::resolve(&store_name, &principal, Some(&alice)).unwrap();
    assert_eq!(
        db.create(&private, &principal, &bob, "tags", "hidden", &tag)
            .await,
        Err(Error::NotFound)
    );
    db.create(&private, &principal, &alice, "tags", "hidden", &tag)
        .await
        .unwrap();
    assert_eq!(
        db.get(&namespace, &definition, "tags", "hidden").await,
        Err(Error::NotFound)
    );

    let link = db
        .link(&namespace, &definition, &alice, "tag", "a", "one")
        .await
        .unwrap();
    assert_eq!(link.attribution, alice_only);
    assert_eq!(
        db.link(&namespace, &definition, &alice, "tag", "b", "one")
            .await,
        Err(Error::Conflict)
    );
    db.link(&namespace, &definition, &bob, "tag", "a", "two")
        .await
        .unwrap();
    let mut outgoing = Traversal {
        limit: 1,
        ..Traversal::default()
    };
    for expected in ["one", "two"] {
        let page = db
            .traverse(&namespace, &definition, "tag", "a", &outgoing)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].record.id, expected);
        outgoing.cursor = page.cursor;
        assert_eq!(outgoing.cursor.is_some(), expected == "one");
    }
    db.link(&namespace, &definition, &alice, "primary", "a", "one")
        .await
        .unwrap();
    for (from, to) in [("a", "two"), ("b", "one")] {
        assert_eq!(
            db.link(&namespace, &definition, &alice, "primary", from, to)
                .await,
            Err(Error::Conflict)
        );
    }
    for (from, to) in [("a", "one"), ("a", "two"), ("b", "one")] {
        db.link(&namespace, &definition, &alice, "related", from, to)
            .await
            .unwrap();
    }
    assert_eq!(
        db.link(&namespace, &definition, &alice, "related", "a", "one")
            .await,
        Err(Error::Conflict)
    );
    let incoming = Traversal {
        direction: Direction::Incoming,
        ..Traversal::default()
    };
    let page = db
        .traverse(&namespace, &definition, "tag", "two", &incoming)
        .await
        .unwrap();
    assert_eq!(page.items[0].record.id, "a");
    assert_eq!(
        page.items[0].link.attribution.created_by.as_deref(),
        Some("bob")
    );

    for bad in ["one", "hidden"] {
        assert!(raw!(&mut *stock,
            "INSERT OR REPLACE INTO links VALUES ($1, 'tag', 'notes', 'b', 'tags', $2, 'one-to-many', NULL, NULL)",
            "INSERT INTO links VALUES ($1, 'tag', 'notes', 'b', 'tags', $2, 'one-to-many', NULL, NULL)",
            part, bad,
        ).is_err());
    }
    assert!(raw!(&mut *stock,
        "INSERT INTO links VALUES ($1, 'tag', 'notes', 'b', 'tags', 'one', 'many-to-many', NULL, NULL)",
        part,
    ).is_err());
    raw!(
        &mut *stock,
        "DELETE FROM records WHERE namespace = $1 AND collection = 'notes' AND id = 'a'",
        part,
    )
    .unwrap();
    assert!(
        db.traverse(&namespace, &definition, "tag", "one", &incoming)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(db.get(&namespace, &definition, "tags", "two").await.is_ok());
    raw!(&mut *stock,
        "INSERT INTO records VALUES ($1, 'notes', 'a', '{\"title\":\"raw recreated\"}', NULL, NULL)",
        part,
    ).unwrap();
    assert_eq!(
        db.get(&namespace, &definition, "notes", "a")
            .await
            .unwrap()
            .attribution,
        Attribution::default()
    );
    raw!(&mut *stock,
        "INSERT INTO links VALUES ($1, 'tag', 'notes', 'b', 'tags', 'one', 'one-to-many', 'forged', 'forged')",
        part,
    ).unwrap();
    assert_eq!(
        db.traverse(&namespace, &definition, "tag", "one", &incoming)
            .await
            .unwrap()
            .items[0]
            .link
            .attribution,
        Attribution::default(),
    );
    db.unlink(&namespace, &definition, &alice, "tag", "b", "one")
        .await
        .unwrap();

    verify_transaction(db, &namespace, &definition, &alice, &store_name).await
}

async fn verify_transaction(
    db: &Relational,
    namespace: &Namespace,
    definition: &Definition,
    alice: &Actor,
    label: &str,
) -> String {
    let note = fields(json!({"title": "pending"}));
    let mut pending = db.begin().await.unwrap();
    pending
        .create(
            namespace,
            definition,
            alice,
            "notes",
            "will-rollback",
            &note,
        )
        .await
        .unwrap();
    assert_eq!(
        db.get(namespace, definition, "notes", "will-rollback")
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(
        pending
            .get(namespace, definition, "notes", "will-rollback")
            .await
            .unwrap()
            .fields,
        note
    );
    assert_eq!(
        pending
            .create(
                namespace,
                definition,
                alice,
                "notes",
                "will-rollback",
                &note
            )
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(
        pending.commit(&format!("{label}-rejected")).await,
        Err(Error::Conflict)
    );
    assert!(!db.has_receipt(&format!("{label}-rejected")).await.unwrap());
    assert_eq!(
        db.get(namespace, definition, "notes", "will-rollback")
            .await,
        Err(Error::NotFound)
    );

    let mut abandoned = db.begin().await.unwrap();
    abandoned
        .create(namespace, definition, alice, "notes", "abandoned", &note)
        .await
        .unwrap();
    drop(abandoned);
    assert_eq!(
        db.get(namespace, definition, "notes", "abandoned").await,
        Err(Error::NotFound)
    );

    let mut rolled_back = db.begin().await.unwrap();
    rolled_back
        .delete(namespace, definition, alice, "notes", "b")
        .await
        .unwrap();
    assert_eq!(
        rolled_back.get(namespace, definition, "notes", "b").await,
        Err(Error::NotFound)
    );
    rolled_back.rollback().await.unwrap();
    assert!(db.get(namespace, definition, "notes", "b").await.is_ok());

    let mut committed = db.begin().await.unwrap();
    committed
        .create(namespace, definition, alice, "notes", "committed", &note)
        .await
        .unwrap();
    committed
        .link(namespace, definition, alice, "tag", "committed", "one")
        .await
        .unwrap();
    let receipt = format!("{label}-commit");
    committed.commit(&receipt).await.unwrap();
    assert!(db.has_receipt(&receipt).await.unwrap());
    assert_eq!(
        db.traverse(
            namespace,
            definition,
            "tag",
            "committed",
            &Traversal::default()
        )
        .await
        .unwrap()
        .items
        .len(),
        1
    );
    let mut duplicate = db.begin().await.unwrap();
    duplicate
        .create(namespace, definition, alice, "notes", "duplicate", &note)
        .await
        .unwrap();
    assert_eq!(duplicate.commit(&receipt).await, Err(Error::Conflict));
    assert_eq!(
        db.get(namespace, definition, "notes", "duplicate").await,
        Err(Error::NotFound)
    );

    let mut first = db.begin().await.unwrap();
    let mut second = db.begin().await.unwrap();
    let predicate = Query {
        filters: vec![Filter {
            field: "title".into(),
            op: FilterOp::Eq,
            value: Some(json!("only one")),
        }],
        ..Query::default()
    };
    assert!(
        first
            .find(namespace, definition, "notes", &predicate)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        second
            .find(namespace, definition, "notes", &predicate)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let competing = fields(json!({"title": "only one"}));
    first
        .create(namespace, definition, alice, "notes", "first", &competing)
        .await
        .unwrap();
    first.commit(&format!("{label}-first")).await.unwrap();
    assert!(
        second
            .find(namespace, definition, "notes", &predicate)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let result = second
        .create(namespace, definition, alice, "notes", "second", &competing)
        .await;
    match result {
        Ok(_) => assert_eq!(
            second.commit(&format!("{label}-second")).await,
            Err(Error::Conflict)
        ),
        Err(Error::Conflict) => second.rollback().await.unwrap(),
        Err(error) => panic!("unexpected conflict result: {error}"),
    }
    assert_eq!(
        db.find(namespace, definition, "notes", &predicate)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    receipt
}
