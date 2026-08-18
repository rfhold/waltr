use super::*;
use crate::resource_catalog::materialize::materialize_url;
use std::{
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(test: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            env::temp_dir().join(format!("waltr-definition-{test}-{}-{nonce}", process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn resource(&self, path: &str, source: &str) {
        let path = self.0.join(".waltr/resources").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

async fn unified_state(directory: &TestDirectory) -> AgentDefinitionState {
    directory.resource(
        "mcp-servers/waltr.yaml",
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: https://waltr.example/base\n  path: /mcp\n",
    );
    let materializer =
        crate::catalog::CatalogMaterializer::load(&crate::test_support::default_catalog_loader())
            .await
            .unwrap();
    AgentDefinitionState::load_unified(&directory.0, materializer).unwrap()
}

fn agent(name: &str, description: &str) -> String {
    format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: {name}\nspec:\n  description: {description}\n"
    )
}

#[tokio::test]
async fn unified_catalog_materializes_all_six_kinds_and_runtime_indexes() {
    let directory = TestDirectory::new("unified-six-kinds");
    let state = unified_state(&directory).await;
    let catalog = state.resource_catalog();
    let kinds = catalog
        .resources
        .iter()
        .map(|resource| resource.kind)
        .collect::<BTreeSet<_>>();
    assert_eq!(kinds.len(), 6);
    assert_eq!(catalog.resources.len(), 12);
    let config = catalog.effective_configuration.unwrap();
    assert_eq!(config.providers.len(), 4);
    assert_eq!(
        config.mcp_servers["waltr"].url,
        "https://waltr.example/base/mcp"
    );
    assert!(
        config.providers["codex"]
            .models
            .contains_key("codex/gpt-5.3")
    );
    assert_eq!(config.inference_defaults.unwrap().provider_key, "codex");
    assert_eq!(catalog.etag, state.snapshot().etag());
}

#[tokio::test]
async fn unified_provider_override_and_reset_use_slot_etags() {
    let directory = TestDirectory::new("unified-precedence-reset");
    let state = unified_state(&directory).await;
    let inspection = state
        .resource_inspections(Some("providers/openai.yaml"))
        .unwrap()
        .remove(0);
    assert!(!inspection.persisted);
    let override_source = "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: openai\nspec:\n  transport: openaiResponses\n  url: https://gateway.example/v1/responses\n";
    state
        .replace(
            "providers/openai.yaml",
            override_source.to_owned(),
            &inspection.persisted_etag,
        )
        .await
        .unwrap();
    assert_eq!(
        state.snapshot().configuration().providers["openai"].url,
        "https://gateway.example/v1/responses"
    );
    let persisted_etag = state
        .resource_inspections(Some("providers/openai.yaml"))
        .unwrap()
        .remove(0)
        .persisted_etag;
    state
        .reset("providers/openai.yaml", &persisted_etag)
        .await
        .unwrap();
    assert_eq!(
        state.snapshot().configuration().providers["openai"].url,
        "https://api.openai.com/v1/chat/completions"
    );
}

#[tokio::test]
async fn invalid_unified_reload_keeps_last_known_good_and_reports_error() {
    let directory = TestDirectory::new("unified-last-known-good");
    let state = unified_state(&directory).await;
    let before = state.snapshot();
    directory.resource(
        "providers/openai.yaml",
        "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: openai\nspec:\n  transport: openaiResponses\n  url: not-a-url\n",
    );
    state.reload_from_disk().await;
    assert!(Arc::ptr_eq(&before, &state.snapshot()));
    assert!(
        state
            .reload_error()
            .unwrap()
            .contains("URL must be absolute")
    );
}

#[test]
fn environment_templates_fail_closed_and_url_joining_is_uri_aware() {
    let error = materialize_url(
        "${env:WALTR_RESOURCE_TEST_VARIABLE_THAT_MUST_NOT_EXIST}",
        "/mcp",
        true,
        "mcp-servers/waltr.yaml",
    )
    .unwrap_err();
    assert!(error.to_string().contains("required environment variable"));
    assert_eq!(
        materialize_url(
            "https://waltr.example/prefix/",
            "/mcp",
            true,
            "mcp-servers/waltr.yaml"
        )
        .unwrap(),
        "https://waltr.example/prefix/mcp"
    );
    let stdio = materialize_mcp_server(
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: local\nspec:\n  transport: stdio\n  command: example-server\n",
        "mcp-servers/local.yaml",
    )
    .unwrap();
    assert_eq!(stdio.r#type, config_proto::McpServerType::Stdio as i32);
    assert_eq!(stdio.command, "example-server");
}

#[test]
fn bundled_catalog_is_split_and_lexically_ordered() {
    let loaded = load_default_agent(&[], "coding").unwrap();
    assert_eq!(loaded.resolved.name, "coding");
    assert_eq!(loaded.bundle.sources.len(), 6);
    assert!(loaded.bundle.sources[0].contains("name: coding"));
    assert!(loaded.bundle.sources[1].contains("name: configuration"));
    assert!(loaded.bundle.sources[2].contains("name: default"));
    assert!(loaded.bundle.resolve().is_ok());
}

#[test]
fn strict_identity_and_one_document_are_required() {
    let directory = TestDirectory::new("identity");
    directory.resource("agents/wrong.yaml", &agent("other", "wrong"));
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::ResourceIdentity { .. })
    ));
    directory.resource(
        "agents/wrong.yaml",
        &format!("{}---\n{}", agent("wrong", "one"), agent("second", "two")),
    );
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::ResourceCount { count: 2, .. })
    ));
}

#[test]
fn whole_file_shadow_preserves_bytes_without_inheritance_and_adds_resources() {
    let directory = TestDirectory::new("shadow");
    let shadow = agent("coding", "standalone");
    let added = agent("extra", "added");
    directory.resource("agents/coding.yaml", &shadow);
    directory.resource("agents/extra.yaml", &added);
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let snapshot = state.snapshot();
    let coding = snapshot.catalog().resolved("coding").unwrap();
    assert_eq!(coding.description.as_deref(), Some("standalone"));
    assert!(coding.features.filesystem.is_none());
    assert!(snapshot.catalog().resolved("extra").is_some());
    assert_eq!(snapshot.sources()[0], shadow);
    assert_eq!(snapshot.sources()[3], added);
}

#[test]
fn external_sources_keep_final_replacement_precedence() {
    let directory = TestDirectory::new("external");
    directory.resource("agents/coding.yaml", &agent("coding", "filesystem"));
    let external = directory.0.join("external.yaml");
    fs::write(&external, agent("coding", "external")).unwrap();
    let state = AgentDefinitionState::load(&directory.0, &[external], "coding").unwrap();
    assert_eq!(
        state
            .snapshot()
            .catalog()
            .resolved("coding")
            .unwrap()
            .description
            .as_deref(),
        Some("external")
    );
}

#[test]
fn invalid_startup_nested_and_source_fail() {
    let directory = TestDirectory::new("invalid-startup");
    directory.resource("agents/bad.yaml", "kind: [");
    assert!(AgentDefinitionState::load(&directory.0, &[], "coding").is_err());
    fs::remove_file(directory.0.join(".waltr/resources/agents/bad.yaml")).unwrap();
    directory.resource("agents/nested/bad.yaml", &agent("bad", "nested"));
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::NestedResource(_))
    ));
}

#[test]
fn startup_rejects_every_unsupported_resource_tree_candidate() {
    for (test, path) in [
        ("yml", "agents/extra.yml"),
        ("uppercase", "agents/extra.YAML"),
        ("extensionless", "agents/extra"),
        ("dotfile", "agents/.ignored"),
        ("tmp", "agents/extra.tmp"),
        ("unknown-category-file", "unknown/extra.yaml"),
    ] {
        let directory = TestDirectory::new(test);
        directory.resource(path, &agent("extra", "unsupported"));
        assert!(matches!(
            AgentDefinitionState::load(&directory.0, &[], "coding"),
            Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
        ));
    }

    let directory = TestDirectory::new("unknown-category-directory");
    fs::create_dir_all(directory.0.join(".waltr/resources/unknown")).unwrap();
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));

    let directory = TestDirectory::new("unknown-category-file");
    fs::create_dir_all(directory.0.join(".waltr/resources")).unwrap();
    fs::write(
        directory.0.join(".waltr/resources/unknown.yaml"),
        agent("unknown", "unsupported"),
    )
    .unwrap();
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));

    let directory = TestDirectory::new("category-file");
    fs::create_dir_all(directory.0.join(".waltr/resources")).unwrap();
    fs::write(
        directory.0.join(".waltr/resources/agents"),
        "not a directory",
    )
    .unwrap();
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));

    let directory = TestDirectory::new("empty-nested-directory");
    fs::create_dir_all(directory.0.join(".waltr/resources/agents/nested")).unwrap();
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::NestedResource(_))
    ));
}

#[test]
fn startup_ignores_only_exact_internal_atomic_temporary_files() {
    let directory = TestDirectory::new("internal-temporary");
    directory.resource(
        "agents/.coding.yaml.01234567-89ab-cdef-0123-456789abcdef.tmp",
        "partial",
    );
    AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();

    directory.resource("agents/.coding.yaml.not-a-uuid.tmp", "partial");
    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));
}

#[cfg(unix)]
#[test]
fn persisted_yaml_symlinks_are_rejected_without_following() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("symlink");
    let target = directory.0.join("target.yaml");
    fs::write(&target, agent("linked", "linked")).unwrap();
    let category = directory.0.join(".waltr/resources/agents");
    fs::create_dir_all(&category).unwrap();
    symlink(&target, category.join("linked.yaml")).unwrap();

    assert!(matches!(
        AgentDefinitionState::load(&directory.0, &[], "coding"),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));
}

#[tokio::test]
async fn replace_rejects_stale_etag_and_reset_falls_back_or_removes() {
    let directory = TestDirectory::new("replace-reset");
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let AgentDefinitionInspection::Resource(detail) =
        state.inspect(Some("agents/coding.yaml")).unwrap()
    else {
        unreachable!()
    };
    let replacement = agent("coding", "replacement");
    let replaced = state
        .replace(
            "agents/coding.yaml",
            replacement.clone(),
            &detail.persisted_etag,
        )
        .await
        .unwrap();
    assert_eq!(replaced.source.as_deref(), Some(replacement.as_str()));
    assert!(matches!(
        state
            .replace(
                "agents/coding.yaml",
                agent("coding", "stale"),
                &detail.persisted_etag
            )
            .await,
        Err(AgentDefinitionLoadError::StaleEtag { .. })
    ));
    let reset = state
        .reset("agents/coding.yaml", &replaced.persisted_etag)
        .await
        .unwrap();
    assert!(!reset.persisted);
    assert_eq!(reset.origin, Some(ResourceOrigin::Bundled));

    let extra = agent("extra", "new");
    let AgentDefinitionInspection::Resource(detail) =
        state.inspect(Some("agents/extra.yaml")).unwrap()
    else {
        unreachable!()
    };
    let absent = detail.persisted_etag;
    let added = state
        .replace("agents/extra.yaml", extra, &absent)
        .await
        .unwrap();
    let removed = state
        .reset("agents/extra.yaml", &added.persisted_etag)
        .await
        .unwrap();
    assert!(removed.source.is_none());
    assert!(state.snapshot().catalog().resolved("extra").is_none());
}

#[tokio::test]
async fn watcher_keeps_last_known_good_then_repairs_and_ignores_api_event() {
    let directory = TestDirectory::new("watcher");
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let watcher = state.spawn_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    directory.resource("agents/coding.yaml", "kind: [");
    wait_until(|| state.reload_error().is_some()).await;
    assert_ne!(state.snapshot().sources()[0], "kind: [");

    let repaired = agent("coding", "repaired");
    directory.resource("agents/coding.yaml", &repaired);
    wait_until(|| state.reload_error().is_none() && state.snapshot().sources()[0] == repaired)
        .await;
    let fingerprint = state.snapshot().persisted_fingerprint().to_owned();
    directory.resource("agents/coding.yaml", &repaired);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(state.snapshot().persisted_fingerprint(), fingerprint);
    assert!(state.reload_error().is_none());
    watcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn watcher_rejects_unsupported_candidates_and_retains_last_known_good() {
    let directory = TestDirectory::new("watcher-unsupported");
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let active = state.snapshot();
    let watcher = state.spawn_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    directory.resource("agents/extra.yml", &agent("extra", "unsupported"));
    wait_until(|| state.reload_error().is_some()).await;
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    assert!(state.snapshot().catalog().resolved("extra").is_none());

    fs::remove_file(directory.0.join(".waltr/resources/agents/extra.yml")).unwrap();
    wait_until(|| state.reload_error().is_none()).await;

    directory.resource("agents/nested/extra.yaml", &agent("extra", "nested"));
    wait_until(|| state.reload_error().is_some()).await;
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    fs::remove_dir_all(directory.0.join(".waltr/resources/agents/nested")).unwrap();
    wait_until(|| state.reload_error().is_none()).await;

    directory.resource("unknown/extra.yaml", &agent("extra", "unknown category"));
    wait_until(|| state.reload_error().is_some()).await;
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    fs::remove_dir_all(directory.0.join(".waltr/resources/unknown")).unwrap();
    wait_until(|| state.reload_error().is_none()).await;

    directory.resource(
        "agents/.coding.yaml.01234567-89ab-cdef-0123-456789abcdef.tmp",
        "partial",
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(state.reload_error().is_none());
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    watcher.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn watcher_rejects_symlink_candidates_and_retains_last_known_good() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("watcher-symlink");
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let active = state.snapshot();
    let watcher = state.spawn_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let target = directory.0.join("target.yaml");
    fs::write(&target, agent("extra", "linked")).unwrap();
    fs::create_dir_all(directory.0.join(".waltr/resources/agents")).unwrap();
    symlink(
        target,
        directory.0.join(".waltr/resources/agents/extra.yaml"),
    )
    .unwrap();

    wait_until(|| state.reload_error().is_some()).await;
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    watcher.shutdown().await.unwrap();
}

#[test]
fn stale_candidate_fingerprint_is_rejected_before_publication() {
    let directory = TestDirectory::new("stale-candidate");
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let observed = discover_persisted(&state.inner.resources_root).unwrap();
    let observed_fingerprint = tree_fingerprint(&observed);
    let candidate = state.build_candidate(&observed).unwrap();

    directory.resource("agents/coding.yaml", &agent("coding", "changed"));

    assert!(matches!(
        state.ensure_tree_unchanged(&observed_fingerprint),
        Err(AgentDefinitionLoadError::ConcurrentFilesystemChange)
    ));
    assert_ne!(
        candidate.persisted_fingerprint(),
        tree_fingerprint(&discover_persisted(&state.inner.resources_root).unwrap())
    );
}

#[test]
fn post_mutation_stabilization_builds_publication_from_actual_disk_tree() {
    let directory = TestDirectory::new("post-mutation-actual-tree");
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let requested = agent("coding", "requested");
    let mut intended_tree = discover_persisted(&state.inner.resources_root).unwrap();
    intended_tree.insert("agents/coding.yaml".to_owned(), requested.clone());
    let precomputed = state.build_candidate(&intended_tree).unwrap();

    directory.resource("agents/coding.yaml", &requested);
    directory.resource("agents/extra.yaml", &agent("extra", "concurrent addition"));
    let stabilized = state
        .stabilize_after_mutation(
            "agents/coding.yaml",
            PersistedSlotPostcondition::Exact(&requested),
        )
        .unwrap();

    assert!(precomputed.catalog().resolved("extra").is_none());
    assert!(stabilized.catalog().resolved("extra").is_some());
    assert_ne!(
        precomputed.persisted_fingerprint(),
        stabilized.persisted_fingerprint()
    );
    state.publish(stabilized);
    assert!(state.snapshot().catalog().resolved("extra").is_some());
}

#[test]
fn post_mutation_stabilization_rejects_lost_target_postcondition() {
    let directory = TestDirectory::new("post-mutation-target-loss");
    let state = AgentDefinitionState::load(&directory.0, &[], "coding").unwrap();
    let active = state.snapshot();
    let requested = agent("coding", "requested");
    let mut intended_tree = discover_persisted(&state.inner.resources_root).unwrap();
    intended_tree.insert("agents/coding.yaml".to_owned(), requested.clone());
    drop(state.build_candidate(&intended_tree).unwrap());

    directory.resource("agents/coding.yaml", &agent("coding", "external winner"));
    let result = state.stabilize_after_mutation(
        "agents/coding.yaml",
        PersistedSlotPostcondition::Exact(&requested),
    );

    assert!(matches!(
        result,
        Err(AgentDefinitionLoadError::ConcurrentFilesystemChange)
    ));
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    assert!(
        state
            .reload_error()
            .is_some_and(|error| error.contains("resource tree changed"))
    );
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async move {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn etags_and_tree_fingerprints_are_exact_and_deterministic() {
    assert_ne!(slot_etag(None), slot_etag(Some(b"")));
    assert_ne!(source_etag(b"a\n"), source_etag(b"a"));
    let first = BTreeMap::from([
        ("agents/a.yaml".to_owned(), "a".to_owned()),
        ("agents/b.yaml".to_owned(), "b".to_owned()),
    ]);
    let second = BTreeMap::from([
        ("agents/b.yaml".to_owned(), "b".to_owned()),
        ("agents/a.yaml".to_owned(), "a".to_owned()),
    ]);
    assert_eq!(tree_fingerprint(&first), tree_fingerprint(&second));
}

#[test]
fn path_validation_rejects_traversal_extensions_and_categories() {
    for path in [
        "/agents/a.yaml",
        "agents/../a.yaml",
        "agents/a.yml",
        "other/a.yaml",
        "agents/nested/a.yaml",
        "agents/.yaml",
    ] {
        assert!(matches!(
            validate_resource_path(path),
            Err(AgentDefinitionLoadError::InvalidResourcePath(_))
        ));
    }
}
