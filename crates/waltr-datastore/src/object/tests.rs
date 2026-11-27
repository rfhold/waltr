use std::collections::BTreeMap;
use std::fs;

use super::*;
use crate::{Access, Definition, Kind, Scope};

fn actor(name: &str) -> Actor {
    Actor::from_trusted_key(name.to_owned()).unwrap()
}

fn namespace(name: &str, scope: Scope, actor: Option<&Actor>) -> Namespace {
    Namespace::resolve(
        name,
        &Definition {
            kind: Kind::Object,
            access: Access { scope },
            collections: BTreeMap::new(),
            relations: BTreeMap::new(),
        },
        actor,
    )
    .unwrap()
}

async fn bytes(store: &Objects, ns: &Namespace, key: &str) -> (ObjectInfo, Vec<u8>) {
    let ObjectRead { info, mut body } = store.read(ns, key).await.unwrap();
    let mut result = Vec::new();
    body.read_to_end(&mut result).await.unwrap();
    (info, result)
}

#[tokio::test]
async fn external_files_are_authoritative_and_receipts_do_not_grant_authorship() {
    let root = tempfile::tempdir().unwrap();
    let store = Objects::filesystem(root.path()).unwrap();
    let alice = actor("alice");
    let bob = actor("bob");
    let ns = namespace("notes", Scope::Shared, None);
    let private = namespace("notes", Scope::Principal, Some(&bob));
    let key = "deep/note.bin";

    assert!(
        store
            .list(&ns, None, None, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let physical = root.path().join(ns.as_str()).join(key);
    fs::create_dir_all(physical.parent().unwrap()).unwrap();
    fs::write(&physical, [0, 255, 31]).unwrap();
    let (info, output) = bytes(&store, &ns, key).await;
    assert_eq!(output, [0, 255, 31]);
    assert_eq!(info.attribution, Attribution::default());
    assert_eq!(info.last_known_waltr_write, None);
    assert_eq!(
        store.list(&ns, Some("deep"), None, 1).await.unwrap().items,
        [info]
    );
    assert_eq!(store.stat(&private, key).await, Err(Error::NotFound));

    fs::write(&physical, [255, 7]).unwrap();
    assert_eq!(bytes(&store, &ns, key).await.1, [255, 7]);
    assert_eq!(
        store.create(&ns, key, &alice, &[1][..]).await,
        Err(Error::Conflict)
    );
    fs::remove_file(&physical).unwrap();
    assert!(
        store
            .list(&ns, Some("deep"), None, 1)
            .await
            .unwrap()
            .items
            .is_empty()
    );

    let created = store.create(&ns, key, &alice, &[0, 128][..]).await.unwrap();
    assert_eq!(
        created.last_known_waltr_write.unwrap().operation,
        ObjectOperation::Create
    );
    let reopened = Objects::filesystem(root.path()).unwrap();
    let replaced = reopened
        .replace(&ns, key, &bob, &[3, 222][..])
        .await
        .unwrap();
    let history = replaced.last_known_waltr_write.as_ref().unwrap();
    assert_eq!(history.attribution.last_edited_by.as_deref(), Some("bob"));
    #[cfg(target_vendor = "apple")]
    {
        assert_eq!(replaced.attribution.created_by.as_deref(), Some("alice"));
        assert_eq!(replaced.attribution.last_edited_by.as_deref(), Some("bob"));
    }
    assert_eq!(bytes(&reopened, &ns, key).await, (replaced, vec![3, 222]));

    fs::write(&physical, [19, 37, 98]).unwrap();
    let (external_edit, output) = bytes(&store, &ns, key).await;
    assert_eq!(output, [19, 37, 98]);
    assert_eq!(external_edit.attribution, Attribution::default());
    assert_eq!(
        external_edit.last_known_waltr_write.unwrap().operation,
        ObjectOperation::Replace
    );
    fs::remove_file(&physical).unwrap();
    fs::write(&physical, [3, 222]).unwrap();
    assert_eq!(
        store.stat(&ns, key).await.unwrap().attribution,
        Attribution::default()
    );
    store.delete(&ns, key, &bob).await.unwrap();
    assert!(!physical.exists());
    fs::write(&physical, [3, 222]).unwrap();
    let external_recreation = store.stat(&ns, key).await.unwrap();
    assert_eq!(external_recreation.attribution, Attribution::default());
    assert_eq!(
        external_recreation
            .last_known_waltr_write
            .unwrap()
            .operation,
        ObjectOperation::Delete
    );
}

#[tokio::test]
async fn bounded_pages_and_paths() {
    let root = tempfile::tempdir().unwrap();
    let store = Objects::filesystem(root.path()).unwrap();
    let actor = actor("alice");
    let ns = namespace("Docs", Scope::Shared, None);
    let alias = namespace("docs", Scope::Shared, None);
    for key in ["nested/z", "nested/a", "nested-a", "z"] {
        store.create(&ns, key, &actor, &b"\0"[..]).await.unwrap();
    }
    let first = store.list(&ns, Some("nested"), None, 1).await.unwrap();
    assert_eq!(first.items[0].key, "nested/a");
    assert_eq!(first.next_after.as_deref(), Some("nested/a"));
    let second = store
        .list(&ns, Some("nested"), first.next_after.as_deref(), 1)
        .await
        .unwrap();
    assert_eq!(second.items[0].key, "nested/z");
    assert_eq!(second.next_after, None);
    let global = store.list(&ns, None, Some("nested-a"), 1).await.unwrap();
    assert_eq!(global.items[0].key, "nested/a");
    assert_eq!(store.stat(&alias, "nested/a").await, Err(Error::NotFound));
    assert_eq!(store.stat(&ns, "Nested/a").await, Err(Error::NotFound));
    assert_eq!(
        store.replace(&ns, "missing", &actor, &b""[..]).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.create(&ns, "nested", &actor, &b""[..]).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        store.create(&ns, "z/child", &actor, &b""[..]).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        store.list(&ns, Some("nested"), Some("z"), 1).await,
        Err(Error::Invalid)
    );
    assert_eq!(
        store.list(&ns, None, None, MAX_PAGE + 1).await,
        Err(Error::Limit)
    );
    for key in [
        "../secret",
        "nested//a",
        "./z",
        "/z",
        "nested/..",
        "bad\\key",
        "CON.txt",
    ] {
        assert_eq!(store.stat(&ns, key).await, Err(Error::Invalid));
    }
}

#[cfg(windows)]
#[tokio::test]
async fn filesystem_never_follows_windows_directory_reparse_points() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let ns = namespace("links", Scope::Shared, None);
    let data = root.path().join(ns.as_str());
    fs::create_dir_all(&data).unwrap();
    fs::write(outside.path().join("secret"), b"no").unwrap();
    if let Err(err) = std::os::windows::fs::symlink_dir(outside.path(), data.join("dir")) {
        assert_eq!(err.raw_os_error(), Some(1314), "{err}");
        // The standard Windows fixture requires Developer Mode or symlink privileges.
        return;
    }
    let store = Objects::filesystem(root.path()).unwrap();
    let alice = actor("alice");
    assert!(
        store
            .list(&ns, None, None, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(store.stat(&ns, "dir/secret").await, Err(Error::NotFound));
    assert_eq!(
        store.create(&ns, "dir/new", &alice, &b"x"[..]).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        store.replace(&ns, "dir/secret", &alice, &b"x"[..]).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.delete(&ns, "dir/secret", &alice).await,
        Err(Error::NotFound)
    );
    assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"no");
    assert!(!outside.path().join("new").exists());
}

#[tokio::test]
async fn receipt_failure_is_uncertain_but_does_not_hide_or_replay_data() {
    let root = tempfile::tempdir().unwrap();
    let store = Objects::filesystem(root.path()).unwrap();
    let alice = actor("alice");
    let ns = namespace("receipt", Scope::Shared, None);
    let key = "file";
    let receipt_path = root
        .path()
        .join(RECEIPTS)
        .join(RECEIPT_VERSION)
        .join(ns.as_str())
        .join(receipt_name(key));
    fs::create_dir_all(&receipt_path).unwrap();

    assert_eq!(
        store.create(&ns, key, &alice, &b"once"[..]).await,
        Err(Error::Uncertain)
    );
    let (info, output) = bytes(&store, &ns, key).await;
    assert_eq!(output, b"once");
    assert_eq!(info.last_known_waltr_write, None);
    assert_eq!(info.attribution, Attribution::default());
    assert_eq!(
        store.create(&ns, key, &alice, &b"twice"[..]).await,
        Err(Error::Conflict)
    );
    assert_eq!(bytes(&store, &ns, key).await.1, b"once");
}

#[tokio::test]
async fn bounded_bytes_and_open_readers() {
    let root = tempfile::tempdir().unwrap();
    let store = Objects::filesystem(root.path()).unwrap();
    let alice = actor("alice");
    let ns = namespace("limits", Scope::Shared, None);
    assert_eq!(
        store
            .create(&ns, "long", &alice, tokio::io::repeat(0))
            .await,
        Err(Error::Limit)
    );
    assert_eq!(store.stat(&ns, "long").await, Err(Error::NotFound));
    store
        .create(&ns, "small", &alice, &b"byte"[..])
        .await
        .unwrap();
    let mut reads = Vec::new();
    for _ in 0..OPEN_OPERATIONS {
        reads.push(store.read(&ns, "small").await.unwrap());
    }
    assert_eq!(store.stat(&ns, "small").await, Err(Error::Limit));
    reads[0].body.read_to_end(&mut Vec::new()).await.unwrap();
    assert!(store.stat(&ns, "small").await.is_ok());
}

#[tokio::test]
async fn concurrent_create_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    let store = Objects::filesystem(root.path()).unwrap();
    let second = Objects::filesystem(root.path()).unwrap();
    let alice = actor("alice");
    let bob = actor("bob");
    let ns = namespace("race", Scope::Shared, None);
    let (a, b) = tokio::join!(
        store.create(&ns, "once", &alice, &b"alice"[..]),
        second.create(&ns, "once", &bob, &b"bob"[..]),
    );
    let expected = match (a, b) {
        (Ok(_), Err(Error::Conflict)) => b"alice".as_slice(),
        (Err(Error::Conflict), Ok(_)) => b"bob".as_slice(),
        other => panic!("unexpected create outcomes: {other:?}"),
    };
    assert_eq!(bytes(&store, &ns, "once").await.1, expected);
}

#[tokio::test]
async fn idle_body_releases_its_permit_at_the_deadline() {
    let operations = Arc::new(Semaphore::new(1));
    let permit = operations.clone().try_acquire_owned().unwrap();
    let mut body = ObjectBody::new(
        Box::new(tokio::io::empty()),
        1,
        permit,
        Instant::now() + Duration::from_millis(25),
    );
    let _permit = timeout(Duration::from_secs(2), operations.acquire())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        body.read_u8().await.unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

#[cfg(target_vendor = "apple")]
#[tokio::test]
async fn verified_read_uses_the_contents_that_earned_the_receipt() {
    let root = tempfile::tempdir().unwrap();
    let store = Objects::filesystem(root.path()).unwrap();
    let alice = actor("alice");
    let ns = namespace("reads", Scope::Shared, None);
    let key = "snapshot";
    store
        .create(&ns, key, &alice, &b"original"[..])
        .await
        .unwrap();
    let ObjectRead { mut body, info } = store.read(&ns, key).await.unwrap();
    assert_eq!(info.attribution.created_by.as_deref(), Some("alice"));
    fs::write(root.path().join(ns.as_str()).join(key), b"external").unwrap();
    let mut output = Vec::new();
    body.read_to_end(&mut output).await.unwrap();
    assert_eq!(output, b"original");
    assert_eq!(bytes(&store, &ns, key).await.1, b"external");
    assert_eq!(
        store.stat(&ns, key).await.unwrap().attribution,
        Attribution::default()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn filesystem_never_follows_symlink_components() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let alice = actor("alice");
    let ns = namespace("links", Scope::Shared, None);
    let data = root.path().join(ns.as_str());
    fs::create_dir_all(&data).unwrap();
    fs::write(outside.path().join("secret"), b"no").unwrap();
    std::os::unix::fs::symlink(outside.path(), data.join("dir")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), data.join("file")).unwrap();
    std::os::unix::fs::symlink(root.path(), outside.path().join("root-link")).unwrap();
    assert_eq!(
        Objects::filesystem(outside.path().join("root-link")).err(),
        Some(Error::Unavailable)
    );
    let store = Objects::filesystem(root.path()).unwrap();
    assert!(
        store
            .list(&ns, None, None, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for key in ["dir/secret", "file"] {
        assert_eq!(store.stat(&ns, key).await, Err(Error::NotFound));
        assert_eq!(
            store.create(&ns, key, &alice, &b"x"[..]).await,
            Err(Error::Conflict)
        );
        assert_eq!(
            store.replace(&ns, key, &alice, &b"x"[..]).await,
            Err(Error::NotFound)
        );
        assert_eq!(store.delete(&ns, key, &alice).await, Err(Error::NotFound));
    }
    assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"no");
}
