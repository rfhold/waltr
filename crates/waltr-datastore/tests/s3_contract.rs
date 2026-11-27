//! Run with `WALTR_S3_TEST_ENDPOINT=http://127.0.0.1:9000` against disposable S3.
//! This test creates and mutates a unique bucket. It never uses ambient credentials.

use std::collections::BTreeMap;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
use tokio::io::AsyncReadExt;
use uuid::Uuid;
use waltr_datastore::object::{ObjectOperation, Objects};
use waltr_datastore::{Access, Actor, Attribution, Definition, Error, Kind, Namespace, Scope};

#[tokio::test]
#[ignore = "requires a disposable loopback S3-compatible service"]
async fn external_mutations_and_versioned_receipts() {
    let endpoint = std::env::var("WALTR_S3_TEST_ENDPOINT").expect("set WALTR_S3_TEST_ENDPOINT");
    let port = endpoint
        .strip_prefix("http://127.0.0.1:")
        .and_then(|port| port.parse::<u16>().ok());
    assert!(
        port.is_some_and(|port| port != 0),
        "the fixture must use loopback HTTP"
    );
    let user =
        std::env::var("WALTR_S3_TEST_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_owned());
    let password =
        std::env::var("WALTR_S3_TEST_SECRET_KEY").unwrap_or_else(|_| "minioadmin".to_owned());
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(user, password, None, None, "s3-contract"))
        .endpoint_url(endpoint)
        .force_path_style(true)
        .build();
    let writer = Client::from_conf(config.clone());
    let bucket = format!("waltr-object-{}", Uuid::new_v4().simple());
    writer.create_bucket().bucket(&bucket).send().await.unwrap();
    let store = Objects::s3(Client::from_conf(config), &bucket, "test").unwrap();
    let ns = Namespace::resolve(
        "files",
        &Definition {
            kind: Kind::Object,
            access: Access {
                scope: Scope::Shared,
            },
            collections: BTreeMap::new(),
            relations: BTreeMap::new(),
        },
        None,
    )
    .unwrap();
    let alice = Actor::from_trusted_key("alice".into()).unwrap();
    let bob = Actor::from_trusted_key("bob".into()).unwrap();
    let unversioned = store
        .create(&ns, "unversioned", &alice, &b"test"[..])
        .await
        .unwrap();
    assert_eq!(unversioned.attribution, Attribution::default());
    assert_eq!(
        unversioned
            .last_known_waltr_write
            .unwrap()
            .attribution
            .created_by
            .as_deref(),
        Some("alice")
    );
    store.delete(&ns, "unversioned", &alice).await.unwrap();
    writer
        .put_bucket_versioning()
        .bucket(&bucket)
        .versioning_configuration(
            VersioningConfiguration::builder()
                .status(BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .unwrap();

    let key = "nested/café%file";
    let physical = format!("test/{}/{key}", ns.as_str());
    assert!(
        store
            .list(&ns, None, None, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for content in [vec![0, 128, 255], vec![99, 0]] {
        writer
            .put_object()
            .bucket(&bucket)
            .key(&physical)
            .body(ByteStream::from(content.clone()))
            .send()
            .await
            .unwrap();
        let mut read = store.read(&ns, key).await.unwrap();
        let mut bytes = Vec::new();
        read.body.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, content);
        assert_eq!(read.info.attribution, Attribution::default());
        assert_eq!(read.info.last_known_waltr_write, None);
        assert_eq!(
            store
                .list(&ns, Some("nested"), None, 1)
                .await
                .unwrap()
                .items,
            [read.info]
        );
    }
    assert_eq!(
        store.create(&ns, key, &alice, &b"conflict"[..]).await,
        Err(Error::Conflict)
    );
    writer
        .delete_object()
        .bucket(&bucket)
        .key(&physical)
        .send()
        .await
        .unwrap();
    assert_eq!(store.stat(&ns, key).await, Err(Error::NotFound));
    assert!(
        store
            .list(&ns, None, None, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );

    let created = store.create(&ns, key, &alice, &b"alice"[..]).await.unwrap();
    assert_eq!(created.attribution.created_by.as_deref(), Some("alice"));
    let edited = store.replace(&ns, key, &bob, &b"bob"[..]).await.unwrap();
    assert_eq!(edited.attribution.created_by.as_deref(), Some("alice"));
    assert_eq!(edited.attribution.last_edited_by.as_deref(), Some("bob"));
    assert_eq!(store.stat(&ns, key).await.unwrap(), edited);

    // An external copy with the same content and metadata still has a distinct object version.
    let copy_source = format!("{bucket}/test/{}/nested/caf%C3%A9%25file", ns.as_str());
    writer
        .copy_object()
        .bucket(&bucket)
        .key(&physical)
        .copy_source(copy_source)
        .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
        .set_metadata(
            writer
                .head_object()
                .bucket(&bucket)
                .key(&physical)
                .send()
                .await
                .unwrap()
                .metadata,
        )
        .send()
        .await
        .unwrap();
    let observed = store.stat(&ns, key).await.unwrap();
    assert_eq!(observed.attribution, Attribution::default());
    assert_eq!(
        observed.last_known_waltr_write.unwrap().operation,
        ObjectOperation::Replace
    );
    store.delete(&ns, key, &bob).await.unwrap();
    assert_eq!(store.stat(&ns, key).await, Err(Error::NotFound));
    writer
        .put_object()
        .bucket(&bucket)
        .key(&physical)
        .body(ByteStream::from_static(b"bob"))
        .send()
        .await
        .unwrap();
    let recreated = store.stat(&ns, key).await.unwrap();
    assert_eq!(recreated.attribution, Attribution::default());
    assert_eq!(
        recreated.last_known_waltr_write.unwrap().operation,
        ObjectOperation::Delete
    );

    // An externally introduced file/prefix collision resolves toward the parent file.
    assert_eq!(
        store.create(&ns, "nested", &alice, &b"x"[..]).await,
        Err(Error::Conflict)
    );
    writer
        .put_object()
        .bucket(&bucket)
        .key(format!("test/{}/nested", ns.as_str()))
        .body(ByteStream::from_static(b"x"))
        .send()
        .await
        .unwrap();
    assert_eq!(store.stat(&ns, key).await, Err(Error::NotFound));
    let page = store.list(&ns, Some("nested"), None, 10).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].key, "nested");
}
