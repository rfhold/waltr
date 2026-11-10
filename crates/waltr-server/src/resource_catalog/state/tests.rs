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

#[tokio::test]
async fn bundled_configuration_agent_enforces_resource_only_hosted_permissions() {
    use crate::{
        agent_host::{AgentPermissionSelector, Capability, HostRuntimeBuilder, ResolvedPermission},
        interaction::InteractionBroker,
        permission::{
            PermissionLevel, PermissionPolicy, PermissionToolSelector, ServerPermissionPolicy,
        },
    };
    use futures_util::{StreamExt as _, stream};
    use serde_json::json;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use waltr_agent::{
        BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest, LlmStream,
        ModelApi, ModelRef, ProviderId, ToolCall, ToolDefinition,
    };

    struct CallsProvider {
        calls: Vec<ToolCall>,
        requests: AtomicUsize,
    }

    impl LlmStream for CallsProvider {
        fn name(&self) -> &'static str {
            "configuration-permission-test"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let mut events = Vec::new();
                if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    events.extend(self.calls.iter().cloned().map(CompletionEvent::ToolCall));
                }
                events.push(CompletionEvent::Finished);
                Ok(Box::pin(stream::iter(events.into_iter().map(Ok))) as BoxCompletionStream)
            })
        }
    }

    let directory = TestDirectory::new("configuration-permissions");
    let state = unified_state(&directory).await;
    let snapshot = state.snapshot();
    let definition = snapshot.catalog().resolved("configuration").unwrap();
    let mcp = definition.features.mcp.as_ref().unwrap();
    assert_eq!(
        mcp.servers,
        crate::agent_host::definition::McpServerSelection::Include(["waltr".to_owned()].into())
    );
    assert!(mcp.resources && mcp.skills && mcp.expose_tools);
    assert!(!mcp.default_permissions && !mcp.instructions && !mcp.prompts);
    assert!(definition.features.filesystem.is_none());
    assert!(definition.features.shell.is_none());
    assert!(definition.features.task.is_none());

    // The factory's compiler is private; these name-only declarations use its
    // exact public policy construction, then exercise the real agent-loop matcher.
    let mut policy = PermissionPolicy::new(PermissionLevel::Deny);
    for entry in &definition.permissions {
        let AgentPermissionSelector::Name(name) = &entry.selector else {
            panic!("configuration must not grant feature-wide permissions");
        };
        let selector = PermissionToolSelector::exact(name);
        match &entry.permission {
            ResolvedPermission::Tool(effect) => {
                policy = policy.with_tool_rule(selector, effect.clone());
            }
            ResolvedPermission::Inputs(rules) => {
                for rule in rules {
                    policy = policy.with_input_rule(selector.clone(), rule.clone());
                }
            }
            ResolvedPermission::Resources(_) => panic!("no host resource permissions"),
        }
    }

    let item = "waltr://resources/agents%2Fcoding.yaml";
    let edit = json!({"uri":item,"expected_revision":"guarded-revision",
        "edits":[{"operation":"replace","old_text":"old","new_text":"new"}]});
    let cases = vec![
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"waltr://resources"}),
            true,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":item}),
            true,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":format!("{item}/source")}),
            true,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"skill://resource-administration/SKILL.md"}),
            true,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"skill://resource-administration/references/resources.md"}),
            true,
        ),
        (
            "waltr__create",
            json!({"action":"resource.create","input":{"uri":item,"source":"strict YAML","expected_revision":"guarded-revision"}}),
            true,
        ),
        ("waltr__edit", edit.clone(), true),
        (
            "waltr__destroy",
            json!({"action":"resource.destroy","input":{"uri":item,"expected_revision":"guarded-revision"}}),
            true,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"waltr://sessions"}),
            false,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"waltr://sessions/session-id"}),
            false,
        ),
        (
            "read_mcp_resource",
            json!({"server":"other","uri":item}),
            false,
        ),
        (
            "read_mcp_resource",
            json!({"server":"other","uri":"skill://resource-administration/SKILL.md"}),
            false,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"skill://hosted-sessions/SKILL.md"}),
            false,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"file:///tmp/private"}),
            false,
        ),
        (
            "read_mcp_resource",
            json!({"server":"waltr","uri":"skill://resource-administration/unknown.md"}),
            false,
        ),
        (
            "waltr__create",
            json!({"action":"session.create","input":{"prompt":"run"}}),
            false,
        ),
        (
            "waltr__create",
            json!({"action":"resource.unknown","input":{}}),
            false,
        ),
        (
            "waltr__destroy",
            json!({"action":"session.wipe","input":{"confirm":true}}),
            false,
        ),
        (
            "waltr__destroy",
            json!({"action":"resource.unknown","input":{}}),
            false,
        ),
        (
            "waltr__edit",
            json!({"uri":"waltr://sessions/session-id","expected_revision":"rev","edits":[]}),
            false,
        ),
        (
            "waltr__edit",
            json!({"uri":"file:///tmp/private","expected_revision":"rev","edits":[]}),
            false,
        ),
        (
            "waltr__edit",
            json!({"action":"resource.edit","input":edit}),
            false,
        ),
        (
            "waltr__execute",
            json!({"action":"session.send","input":{"id":"id","prompt":"run"}}),
            false,
        ),
        (
            "waltr__execute",
            json!({"action":"session.interrupt","input":{"id":"id"}}),
            false,
        ),
        (
            "waltr__query",
            json!({"action":"session.wait","input":{"id":"id","jq":"true"}}),
            false,
        ),
        (
            "waltr__exec",
            json!({"action":"resources.replace","input":{}}),
            false,
        ),
    ];
    let calls = cases
        .iter()
        .enumerate()
        .map(|(index, (name, arguments, _))| {
            ToolCall::new(format!("case-{index}"), *name, arguments.clone())
        })
        .collect();
    let provider = Arc::new(CallsProvider {
        calls,
        requests: AtomicUsize::new(0),
    });
    let executed = Arc::new(Mutex::new(Vec::new()));
    let sessions = waltr_agent::SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
    let id = sessions
        .create(waltr_agent::SessionCreateRequest::new(
            waltr_agent::SessionId::generate(),
        ))
        .await
        .unwrap()
        .id()
        .clone();
    let mut builder = HostRuntimeBuilder::new(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        provider.clone(),
    )
    .with_permission_policy(policy.clone())
    .with_dispatch_context(
        Some(id.clone()),
        ServerPermissionPolicy::sessions(InteractionBroker::new()),
    );
    for name in [
        "read_mcp_resource",
        "waltr__create",
        "waltr__edit",
        "waltr__destroy",
        "waltr__execute",
        "waltr__query",
        "waltr__exec",
    ] {
        let executed = executed.clone();
        builder = builder.with_tool(Capability::new(
            ToolDefinition::new(
                name,
                "Record authorized dispatch only",
                json!({"type":"object"}),
            ),
            move |context| {
                executed.lock().unwrap().push(context.call().id.clone());
                Ok(context.result("authorized"))
            },
        ));
    }
    let runtime = builder.build();
    let events = sessions
        .prompt(&runtime, id, "Check configuration permissions")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert_eq!(
        provider.requests.load(Ordering::SeqCst),
        2,
        "must finish, not ask for session authority"
    );
    let mut actual = executed.lock().unwrap().clone();
    actual.sort();
    let mut expected = cases
        .iter()
        .enumerate()
        .filter(|(_, (_, _, allowed))| *allowed)
        .map(|(index, _)| format!("case-{index}"))
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);
    for name in [
        "waltr__execute",
        "waltr__query",
        "waltr__exec",
        "shell",
        "read_file",
    ] {
        assert_eq!(policy.evaluate_tool(name), PermissionLevel::Deny, "{name}");
    }
}

async fn explicit_embedded_state(
    directory: &TestDirectory,
) -> Result<AgentDefinitionState, AgentDefinitionLoadError> {
    let source = mcp_server("http://127.0.0.1:43123/mcp");
    crate::resource_catalog::provision_persisted_resource(
        &directory.0,
        "mcp-servers/waltr.yaml",
        &source,
    )?;
    let materializer =
        crate::catalog::CatalogMaterializer::load(&crate::test_support::default_catalog_loader())
            .await
            .unwrap();
    AgentDefinitionState::load_unified_explicit(&directory.0, materializer, "coding")
}

#[cfg(unix)]
#[test]
fn provisioning_rejects_symlinked_catalog_components_without_external_writes() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("provision-symlinks");
    let source = mcp_server("http://127.0.0.1:43123/mcp");

    let external_root = directory.0.join("external-root");
    let external_root_target = external_root.join(".waltr/resources/mcp-servers/waltr.yaml");
    fs::create_dir_all(external_root_target.parent().unwrap()).unwrap();
    fs::write(&external_root_target, "external root").unwrap();
    let linked_root = directory.0.join("linked-root");
    symlink(&external_root, &linked_root).unwrap();
    assert!(matches!(
        crate::resource_catalog::provision_persisted_resource(
            &linked_root,
            "mcp-servers/waltr.yaml",
            &source
        ),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));
    assert_eq!(
        fs::read_to_string(external_root_target).unwrap(),
        "external root"
    );

    let resources_root = directory.0.join("resources-root");
    let external_resources = directory.0.join("external-resources");
    fs::create_dir_all(resources_root.join(".waltr")).unwrap();
    fs::create_dir_all(external_resources.join("mcp-servers")).unwrap();
    let external_resources_target = external_resources.join("mcp-servers/waltr.yaml");
    fs::write(&external_resources_target, "external resources").unwrap();
    symlink(&external_resources, resources_root.join(".waltr/resources")).unwrap();
    assert!(matches!(
        crate::resource_catalog::provision_persisted_resource(
            &resources_root,
            "mcp-servers/waltr.yaml",
            &source
        ),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));
    assert_eq!(
        fs::read_to_string(external_resources_target).unwrap(),
        "external resources"
    );

    let category_root = directory.0.join("category-root");
    let external_category = directory.0.join("external-category");
    fs::create_dir_all(category_root.join(".waltr/resources")).unwrap();
    fs::create_dir(&external_category).unwrap();
    let external_category_target = external_category.join("waltr.yaml");
    fs::write(&external_category_target, "external category").unwrap();
    symlink(
        &external_category,
        category_root.join(".waltr/resources/mcp-servers"),
    )
    .unwrap();
    assert!(matches!(
        crate::resource_catalog::provision_persisted_resource(
            &category_root,
            "mcp-servers/waltr.yaml",
            &source
        ),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));
    assert_eq!(
        fs::read_to_string(external_category_target).unwrap(),
        "external category"
    );

    let path_root = directory.0.join("path-root");
    let external_file = directory.0.join("external-file.yaml");
    fs::create_dir_all(path_root.join(".waltr/resources/mcp-servers")).unwrap();
    fs::write(&external_file, "external path").unwrap();
    symlink(
        &external_file,
        path_root.join(".waltr/resources/mcp-servers/waltr.yaml"),
    )
    .unwrap();
    assert!(matches!(
        crate::resource_catalog::provision_persisted_resource(
            &path_root,
            "mcp-servers/waltr.yaml",
            &source
        ),
        Err(AgentDefinitionLoadError::UnsupportedResourcePath(_))
    ));
    assert_eq!(fs::read_to_string(external_file).unwrap(), "external path");
}

#[tokio::test]
async fn embedded_catalog_loads_ordinary_shell_and_stdio_resources() {
    let directory = TestDirectory::new("embedded-ordinary-resources");
    directory.resource(
        "mcp-servers/process.yaml",
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: process\nspec:\n  transport: stdio\n  command: helper --stdio\n",
    );
    let state = explicit_embedded_state(&directory).await.unwrap();
    let snapshot = state.snapshot();
    let coding = snapshot
        .entries
        .get("features/coding.yaml")
        .expect("ordinary coding feature");
    assert!(coding.source.contains("  shell:"));
    assert!(
        snapshot
            .catalog()
            .resolved("coding")
            .expect("coding agent")
            .features
            .shell
            .is_some()
    );
    let process = snapshot
        .configuration()
        .mcp_servers
        .get("process")
        .expect("stdio MCP server");
    assert_eq!(process.r#type, config_proto::McpServerType::Stdio as i32);
    assert_eq!(process.command, "helper --stdio");
    let self_mcp = snapshot
        .configuration()
        .mcp_servers
        .get("waltr")
        .expect("self MCP");
    assert_eq!(self_mcp.url, "http://127.0.0.1:43123/mcp");
}

#[tokio::test]
async fn provisioned_self_mcp_is_an_ordinary_filesystem_resource() {
    let directory = TestDirectory::new("provisioned-self-mcp");
    directory.resource(
        "mcp-servers/waltr.yaml",
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: stdio\n  command: hostile-override\n",
    );
    let state = explicit_embedded_state(&directory).await.unwrap();
    let configured = state
        .snapshot()
        .configuration()
        .mcp_servers
        .get("waltr")
        .cloned()
        .unwrap();
    assert_eq!(
        configured.r#type,
        config_proto::McpServerType::StreamableHttp as i32
    );
    assert_eq!(configured.url, "http://127.0.0.1:43123/mcp");
    assert!(configured.command.is_empty());

    let expected_source = mcp_server("http://127.0.0.1:43123/mcp");
    assert_eq!(
        fs::read_to_string(directory.0.join(".waltr/resources/mcp-servers/waltr.yaml")).unwrap(),
        expected_source
    );
    let expected_etag = source_etag(expected_source.as_bytes());
    let AgentDefinitionInspection::Resource(detail) = state
        .inspect(Some("mcp-servers/waltr.yaml"))
        .expect("inspect provisioned self MCP")
    else {
        panic!("path inspection must return resource detail");
    };
    assert_eq!(detail.source.as_deref(), Some(expected_source.as_str()));
    assert_eq!(
        detail.persisted_source.as_deref(),
        Some(expected_source.as_str())
    );
    assert_eq!(
        detail.effective_etag.as_deref(),
        Some(expected_etag.as_str())
    );
    assert_eq!(detail.origin, Some(ResourceOrigin::Filesystem));
    let AgentDefinitionInspection::Catalog { entries, .. } = state.inspect(None).unwrap() else {
        panic!("catalog inspection must return summaries");
    };
    let summary = entries
        .iter()
        .find(|entry| entry.path == "mcp-servers/waltr.yaml")
        .expect("self MCP summary");
    assert_eq!(summary.effective_etag, expected_etag);
    assert_eq!(summary.origin, ResourceOrigin::Filesystem);

    let public = state
        .resource_inspections(Some("mcp-servers/waltr.yaml"))
        .unwrap()
        .remove(0);
    assert_eq!(
        public.active_source.as_deref(),
        Some(expected_source.as_str())
    );
    assert!(public.persisted_source.is_some());

    let replacement = mcp_server("https://override.example/mcp");
    let replaced = state
        .replace(
            "mcp-servers/waltr.yaml",
            replacement.clone(),
            &public.persisted_etag,
        )
        .await
        .unwrap();
    assert_eq!(replaced.source.as_deref(), Some(replacement.as_str()));
    assert_eq!(replaced.origin, Some(ResourceOrigin::Filesystem));
    assert!(matches!(
        state
            .replace(
                "mcp-servers/waltr.yaml",
                expected_source,
                &public.persisted_etag
            )
            .await,
        Err(AgentDefinitionLoadError::StaleEtag { .. })
    ));

    let path = "agents/ordinary.yaml";
    let inspection = state.resource_inspections(Some(path)).unwrap().remove(0);
    state
        .replace(
            path,
            agent("ordinary", "ordinary resources remain writable"),
            &inspection.persisted_etag,
        )
        .await
        .unwrap();
    assert!(state.snapshot().catalog().resolved("ordinary").is_some());
}

#[tokio::test]
async fn direct_self_mcp_edits_follow_ordinary_reload_behavior() {
    let directory = TestDirectory::new("self-mcp-direct-edit");
    let state = explicit_embedded_state(&directory).await.unwrap();
    let watcher = state.spawn_watcher().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let replacement = mcp_server("https://direct.example/mcp");
    directory.resource("mcp-servers/waltr.yaml", &replacement);
    wait_until(|| {
        state.snapshot().configuration().mcp_servers["waltr"].url == "https://direct.example/mcp"
    })
    .await;
    let active = state.snapshot();

    directory.resource("mcp-servers/waltr.yaml", "kind: [");
    wait_until(|| state.reload_error().is_some()).await;
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    assert_eq!(
        state.snapshot().configuration().mcp_servers["waltr"].url,
        "https://direct.example/mcp"
    );
    watcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn standalone_self_mcp_resource_remains_mutable() {
    let directory = TestDirectory::new("standalone-self-mcp-mutable");
    let state = unified_state(&directory).await;
    let inspection = state
        .resource_inspections(Some("mcp-servers/waltr.yaml"))
        .unwrap()
        .remove(0);
    let replacement = "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: https://standalone.example/mcp\n";
    let replaced = state
        .replace(
            "mcp-servers/waltr.yaml",
            replacement.to_owned(),
            &inspection.persisted_etag,
        )
        .await
        .expect("standalone self MCP replacement");
    assert_eq!(replaced.source.as_deref(), Some(replacement));
    assert!(replaced.persisted);
    assert_eq!(replaced.origin, Some(ResourceOrigin::Filesystem));
    assert_eq!(
        state.snapshot().configuration().mcp_servers["waltr"].url,
        "https://standalone.example/mcp"
    );
}

fn mcp_server(url: &str) -> String {
    let url = serde_json::to_string(url).unwrap();
    format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: {url}\n"
    )
}

#[tokio::test]
async fn reset_validates_the_complete_fallback_candidate_before_removal() {
    let directory = TestDirectory::new("reset-full-candidate-validation");
    directory.resource(
        "agents/only.yaml",
        &agent("only", "required explicit default"),
    );
    let state = AgentDefinitionState::load(&directory.0, &[], "only").unwrap();
    let inspection = state
        .resource_inspections(Some("agents/only.yaml"))
        .unwrap()
        .remove(0);

    assert!(matches!(
        state
            .reset("agents/only.yaml", &inspection.persisted_etag)
            .await,
        Err(AgentDefinitionLoadError::Resolve { .. })
    ));
    assert!(
        directory
            .0
            .join(".waltr/resources/agents/only.yaml")
            .is_file()
    );
    assert!(state.snapshot().catalog().resolved("only").is_some());
}

#[test]
fn self_mcp_reset_without_public_url_keeps_persisted_and_active_state() {
    const CHILD: &str = "WALTR_TEST_SELF_MCP_RESET_CHILD";
    if env::var_os(CHILD).is_none() {
        let status = process::Command::new(env::current_exe().unwrap())
            .arg("resource_catalog::state::tests::self_mcp_reset_without_public_url_keeps_persisted_and_active_state")
            .arg("--exact")
            .env(CHILD, "1")
            .env_remove("WALTR_PUBLIC_URL")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let directory = TestDirectory::new("self-mcp-reset-no-public-url");
            let state = explicit_embedded_state(&directory).await.unwrap();
            let before = state.snapshot();
            let inspection = state
                .resource_inspections(Some("mcp-servers/waltr.yaml"))
                .unwrap()
                .remove(0);

            assert!(matches!(
                state
                    .reset("mcp-servers/waltr.yaml", &inspection.persisted_etag)
                    .await,
                Err(AgentDefinitionLoadError::InvalidResourceValue { path, .. })
                    if path == "mcp-servers/waltr.yaml"
            ));
            assert!(Arc::ptr_eq(&before, &state.snapshot()));
            assert_eq!(
                fs::read_to_string(directory.0.join(".waltr/resources/mcp-servers/waltr.yaml"))
                    .unwrap(),
                inspection.persisted_source.unwrap()
            );
        });
}

fn agent(name: &str, description: &str) -> String {
    format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: {name}\nspec:\n  description: {description}\n"
    )
}

#[tokio::test]
async fn session_dictation_defaults_validate_and_reload_atomically() {
    let directory = TestDirectory::new("session-dictation-defaults");
    let state = unified_state(&directory).await;
    let original = state.resource_catalog().effective_configuration.unwrap();
    assert!(original.session_dictation.is_none());
    let defaults = format!(
        "{}\n  sessionDictation:\n    routine: audio-transcribe\n    trigger: dictate\n    input: audio\n    output: transcript\n",
        state
            .resource_catalog()
            .resources
            .iter()
            .find(|resource| resource.path == "defaults/default.yaml")
            .unwrap()
            .active_source
    );
    directory.resource("providers/speech.yaml", "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: speech\nspec:\n  transport: googleSpeechV2\n  url: http://localhost:50051\n  models:\n    dictation:\n      name: Dictation\n      type: 3\n      speech_capabilities:\n        unary: true\n        streaming: true\n      recognition:\n        recognizer: projects/test/locations/global/recognizers/_\n        model: chirp_3\n        language_codes: [en-US]\n        sample_rate_hz: 24000\n        channels: 1\n        interim_results: true\n        automatic_punctuation: true\n");
    let routine = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/examples/routines/audio-transcribe.yaml"
    ));
    directory.resource("routines/audio-transcribe.yaml", routine);
    directory.resource("defaults/default.yaml", &defaults);
    state.reload_from_disk().await;
    assert!(state.reload_error().is_none(), "{:?}", state.reload_error());
    let config = state.resource_catalog().effective_configuration.unwrap();
    assert_eq!(config.inference_defaults, original.inference_defaults);
    assert_eq!(
        config.session_dictation.as_ref().unwrap().routine,
        "audio-transcribe"
    );
    let before = state.snapshot();
    for invalid in [
        defaults.replace("input: audio", "input: missing"),
        defaults.replace("trigger: dictate", "trigger: missing"),
        defaults.replace("routine: audio-transcribe", "routine: missing"),
        defaults.replace("output: transcript", "output: bad/identifier"),
        defaults.replace(
            "output: transcript",
            &format!("output: {}", "x".repeat(129)),
        ),
    ] {
        directory.resource("defaults/default.yaml", &invalid);
        state.reload_from_disk().await;
        assert!(state.reload_error().is_some());
        assert!(Arc::ptr_eq(&before, &state.snapshot()));
    }
    directory.resource("defaults/default.yaml", &defaults);
    directory.resource(
        "routines/audio-transcribe.yaml",
        &routine.replace("pcm16le-24000-mono", "pcm16le-16000-mono"),
    );
    state.reload_from_disk().await;
    assert!(state.reload_error().is_some());
    assert!(Arc::ptr_eq(&before, &state.snapshot()));
    let mut scalar: serde_json::Value = serde_yaml_ng::from_str(routine).unwrap();
    scalar["spec"]["parameters"]["schema"] = serde_json::json!({
        "type":"object", "properties":{"language":{"type":"string","maxLength":16}}, "required":[]
    });
    directory.resource(
        "routines/audio-transcribe.yaml",
        &serde_json::to_string(&scalar).unwrap(),
    );
    state.reload_from_disk().await;
    assert!(state.reload_error().is_none(), "{:?}", state.reload_error());
    let with_scalar = state.snapshot();
    scalar["spec"]["parameters"]["schema"]["required"] = serde_json::json!(["language"]);
    directory.resource(
        "routines/audio-transcribe.yaml",
        &serde_json::to_string(&scalar).unwrap(),
    );
    state.reload_from_disk().await;
    assert!(state.reload_error().is_some());
    assert!(Arc::ptr_eq(&with_scalar, &state.snapshot()));
    directory.resource("routines/audio-transcribe.yaml", routine);
    directory.resource("defaults/default.yaml", &defaults.replace("  sessionDictation:\n    routine: audio-transcribe\n    trigger: dictate\n    input: audio\n    output: transcript\n", "  sessionDictation: null\n"));
    state.reload_from_disk().await;
    assert!(state.reload_error().is_none());
    assert!(
        state
            .resource_catalog()
            .effective_configuration
            .unwrap()
            .session_dictation
            .is_none()
    );
}

#[tokio::test]
async fn all_eleven_bundled_js_workflows_match_examples_and_named_helpers() {
    let directory = TestDirectory::new("all-js-workflow-contracts");
    let state = unified_state(&directory).await;
    let snapshot = state.snapshot();
    let expected = [
        "title",
        "compact-and-continue",
        "fork",
        "audio-transcribe",
        "audio-transcribe-cleanup",
        "audio-conversation-notes",
        "audio-synthesize",
        "context-categorization",
        "context-management",
        "manage-category-context",
        "configuration",
    ];
    assert_eq!(snapshot.scripts().len(), expected.len());
    for name in expected {
        let resource = fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("resources/routines/{name}.yaml")),
        )
        .unwrap();
        let example = fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../docs/examples/routines/{name}.yaml")),
        )
        .unwrap();
        assert_eq!(resource, example, "example contract drift: {name}");
        let program = snapshot.scripts().get(name).unwrap().envelope();
        assert!(!program.spec.source.contains("kind: 'restricted'"));
        for capability in &program.spec.capabilities {
            assert!(!capability.starts_with("helper.restricted."));
            if let Some(helper) = capability.strip_prefix("helper.named.") {
                let resolved = snapshot.catalog.resolved(helper).expect("named helper");
                if resolved
                    .system_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains("_helper_capture"))
                {
                    assert_eq!(resolved.permissions.len(), 1, "{helper}");
                    assert!(resolved.permissions.iter().any(|entry| {
                        matches!(&entry.selector, crate::agent_host::definition::AgentPermissionSelector::Name(name) if name == "_helper_capture")
                            && matches!(&entry.permission, crate::agent_host::definition::ResolvedPermission::Tool(effect) if effect == &crate::permission::PermissionEffect::allow())
                    }), "named capture helper lacks its exact tool permission: {helper}");
                }
            }
        }
    }
}

#[tokio::test]
async fn unified_catalog_materializes_all_seven_kinds_and_runtime_indexes() {
    let directory = TestDirectory::new("unified-seven-kinds");
    let state = unified_state(&directory).await;
    let catalog = state.resource_catalog();
    let kinds = catalog
        .resources
        .iter()
        .map(|resource| resource.kind)
        .collect::<BTreeSet<_>>();
    assert_eq!(kinds.len(), 7);
    assert_eq!(catalog.resources.len(), 33);
    let config = catalog.effective_configuration.unwrap();
    assert_eq!(config.providers.len(), 4);
    assert_eq!(
        config.mcp_servers["waltr"].url,
        "https://waltr.example/base/mcp"
    );
    assert!(config.providers["codex"].models.contains_key("gpt-5.3"));
    assert_eq!(config.inference_defaults.unwrap().provider_key, "codex");
    assert_eq!(catalog.etag, state.snapshot().etag());
    let snapshot = state.snapshot();
    let routine = snapshot.scripts().get("title").unwrap();
    assert_eq!(routine.envelope().name, "title");
    assert!(matches!(
        routine.envelope().spec.entrypoints["first-user-message"].trigger,
        crate::routine::definition::Trigger::Event
    ));
    assert!(
        routine
            .envelope()
            .spec
            .capabilities
            .contains("helper.named.routine-title")
    );
    let compact = snapshot.scripts().get("compact-and-continue").unwrap();
    assert!(matches!(
        compact.envelope().spec.entrypoints["context-pressure"].trigger,
        crate::routine::definition::Trigger::Lifecycle
    ));
    assert!(catalog.resources.iter().any(|resource| {
        resource.path == "routines/compact-and-continue.yaml"
            && resource.name == "compact-and-continue"
            && resource.origin == resource_proto::ResourceOrigin::Bundled as i32
            && resource.effective_value.is_some()
    }));
    assert_eq!(snapshot.scripts().len(), 11);
    assert_eq!(snapshot.sources().len(), 16);
    assert!(
        snapshot
            .sources()
            .iter()
            .all(|source| !source.contains("kind: Routine"))
    );
    assert!(
        snapshot
            .loaded()
            .bundle
            .sources
            .iter()
            .all(|source| !source.contains("kind: Routine"))
    );
}

#[tokio::test]
async fn routine_override_reset_and_invalid_reload_preserve_catalog_semantics() {
    let directory = TestDirectory::new("routine-precedence-reset");
    let state = unified_state(&directory).await;
    let bundled = state
        .snapshot()
        .scripts()
        .get("title")
        .unwrap()
        .revision()
        .to_owned();
    let inspection = state
        .resource_inspections(Some("routines/title.yaml"))
        .unwrap()
        .remove(0);
    let replacement = include_str!("../../../resources/routines/title.yaml")
        .replace("Generate and apply", "Generate, validate, and apply");
    state
        .replace(
            "routines/title.yaml",
            replacement,
            &inspection.persisted_etag,
        )
        .await
        .unwrap();
    let replaced = state
        .snapshot()
        .scripts()
        .get("title")
        .unwrap()
        .revision()
        .to_owned();
    assert_ne!(replaced, bundled);

    let persisted_etag = state
        .resource_inspections(Some("routines/title.yaml"))
        .unwrap()
        .remove(0)
        .persisted_etag;
    state
        .reset("routines/title.yaml", &persisted_etag)
        .await
        .unwrap();
    assert_eq!(
        state.snapshot().scripts().get("title").unwrap().revision(),
        bundled
    );

    let active = state.snapshot();
    directory.resource(
        "routines/title.yaml",
        &include_str!("../../../resources/routines/title.yaml").replace(
            "export default async function ()",
            "export default async function (",
        ),
    );
    state.reload_from_disk().await;
    assert!(Arc::ptr_eq(&active, &state.snapshot()));
    assert!(state.reload_error().is_some());
}

#[tokio::test]
async fn production_routine_gate_rejects_legacy_before_write_or_publication() {
    let directory = TestDirectory::new("javascript-production-gate");
    let state = unified_state(&directory).await;
    let original = state.snapshot();
    let inspection = state
        .resource_inspections(Some("routines/title.yaml"))
        .unwrap()
        .remove(0);
    let legacy = "apiVersion: waltr.dev/v1alpha1\nkind: Routine\nmetadata:\n  name: title\nspec:\n  tasks: []\n";
    assert!(
        state
            .replace(
                "routines/title.yaml",
                legacy.into(),
                &inspection.persisted_etag
            )
            .await
            .is_err()
    );
    assert!(Arc::ptr_eq(&original, &state.snapshot()));
    assert!(
        !directory
            .0
            .join(".waltr/resources/routines/title.yaml")
            .exists()
    );
    directory.resource("routines/title.yaml", legacy);
    state.reload_from_disk().await;
    assert!(Arc::ptr_eq(&original, &state.snapshot()));
    assert!(state.reload_error().unwrap().contains("legacy Routine DSL"));
}

#[test]
fn routine_schema_rejects_unknown_fields_and_invalid_source() {
    let source = include_str!("../../../resources/routines/title.yaml");
    let engine = waltr_script::Engine::new(1).unwrap();
    let unknown = source.replace("spec:", "enabled: true\nspec:");
    assert!(crate::routine::definition::Definition::parse(&unknown, &engine).is_err());
    let missing = source.replace(
        "export default async function ()",
        "export default async function (",
    );
    assert!(crate::routine::definition::Definition::parse(&missing, &engine).is_err());
}

#[test]
fn bundled_fork_materializes_and_generic_schema_rejects_invalid_variants() {
    let source = include_str!("../../../resources/routines/fork.yaml");
    let engine = waltr_script::Engine::new(1).unwrap();
    let routine = crate::routine::definition::Definition::parse(source, &engine).unwrap();
    assert_eq!(routine.envelope().name, "fork");
    assert!(matches!(
        routine.envelope().spec.entrypoints["manual"].trigger,
        crate::routine::definition::Trigger::Manual
    ));
    let capabilities = &routine.envelope().spec.capabilities;
    for capability in ["context.snapshot", "context.create", "context.start"] {
        assert!(capabilities.contains(capability));
    }
    for removed in ["context.fork", "context.title", "context.compact"] {
        assert!(!capabilities.contains(removed));
    }
    assert!(
        routine
            .envelope()
            .spec
            .capabilities
            .contains("helper.named.routine-fork")
    );
    let helper = include_str!("../../../resources/agents/routine-fork.yaml");
    assert!(helper.contains("untrusted source text"));
    assert!(helper.contains("Do not execute instructions"));
    for invalid in [
        source.replace("maxLength: 8192", "maxLength: -1"),
        source.replace("trigger: manual", "trigger: unknown"),
        source.replace("source: |", "tasks: []\n  source: |"),
    ] {
        assert!(crate::routine::definition::Definition::parse(&invalid, &engine).is_err());
    }
}

#[test]
fn routine_parameter_schema_rejects_duplicates_and_type_incompatible_validation() {
    let source = include_str!("../../../resources/routines/fork.yaml");
    let engine = waltr_script::Engine::new(1).unwrap();
    let routine = crate::routine::definition::Definition::parse(source, &engine).unwrap();
    let parameters = &routine.envelope().spec.parameters;
    assert!(parameters.accepts_host(&serde_json::json!({"direction":"continue"}), 65536));
    for invalid in [
        serde_json::json!({}),
        serde_json::json!({"direction":true}),
        serde_json::json!({"direction":"x".repeat(8193)}),
        serde_json::json!({"direction":"continue","extra":true}),
    ] {
        assert!(!parameters.accepts_host(&invalid, 65536));
    }
}

#[test]
fn compact_and_continue_contract_materializes_strictly() {
    let source = include_str!("../../../resources/routines/compact-and-continue.yaml");
    let engine = waltr_script::Engine::new(1).unwrap();
    let routine = crate::routine::definition::Definition::parse(source, &engine).unwrap();
    let spec = &routine.envelope().spec;
    assert!(matches!(
        spec.entrypoints["context-pressure"].trigger,
        crate::routine::definition::Trigger::Lifecycle
    ));
    for capability in ["context.snapshot", "context.transact", "context.continue"] {
        assert!(spec.capabilities.contains(capability));
    }
    for removed in ["context.compact", "context.fork", "context.title"] {
        assert!(!spec.capabilities.contains(removed));
    }
    assert!(
        spec.capabilities
            .contains("helper.named.routine-compaction")
    );
    assert!(spec.parameters.accepts_host(&serde_json::json!({}), 65536));
    assert!(
        spec.parameters
            .accepts_host(&serde_json::json!({"continue":true}), 65536)
    );
    assert!(
        !spec
            .parameters
            .accepts_host(&serde_json::json!({"continue":"yes"}), 65536)
    );
    let helper = include_str!("../../../resources/agents/routine-compaction.yaml");
    assert!(helper.contains("features: []"));
    assert!(helper.contains("permissions: []"));
}

fn mixed_trigger_routine() -> &'static str {
    include_str!("../../../resources/routines/compact-and-continue.yaml")
}

#[test]
fn mixed_entrypoints_keep_distinct_typed_trigger_contracts() {
    let engine = waltr_script::Engine::new(1).unwrap();
    let routine =
        crate::routine::definition::Definition::parse(mixed_trigger_routine(), &engine).unwrap();
    assert!(matches!(
        routine.envelope().spec.entrypoints["manual"].trigger,
        crate::routine::definition::Trigger::Manual
    ));
    assert!(matches!(
        routine.envelope().spec.entrypoints["context-pressure"].trigger,
        crate::routine::definition::Trigger::Lifecycle
    ));
    let invalid =
        mixed_trigger_routine().replace("trigger: lifecycle", "trigger: lifecycle\n      when: []");
    assert!(crate::routine::definition::Definition::parse(&invalid, &engine).is_err());
}

fn routine_with_legacy_field(mutations: &str) -> String {
    include_str!("../../../resources/routines/title.yaml")
        .replace("spec:\n", &format!("spec:\n  {mutations}\n"))
}

#[tokio::test]
async fn routine_program_rejects_task_graph_fields_before_publication() {
    let zero = routine_with_legacy_field("tasks: []");
    let two = routine_with_legacy_field("workGroups: []");
    for source in [&zero, &two] {
        let engine = waltr_script::Engine::new(1).unwrap();
        assert!(crate::routine::definition::Definition::parse(source, &engine).is_err());
    }

    let startup = TestDirectory::new("routine-mutation-startup");
    startup.resource(
        "mcp-servers/waltr.yaml",
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: https://waltr.example/base\n  path: /mcp\n",
    );
    startup.resource("routines/title.yaml", &two);
    let materializer =
        crate::catalog::CatalogMaterializer::load(&crate::test_support::default_catalog_loader())
            .await
            .unwrap();
    assert!(AgentDefinitionState::load_unified(&startup.0, materializer).is_err());

    let directory = TestDirectory::new("routine-mutation-replace");
    let state = unified_state(&directory).await;
    let active = state.snapshot();
    let persisted_etag = state
        .resource_inspections(Some("routines/title.yaml"))
        .unwrap()
        .remove(0)
        .persisted_etag;
    for source in [zero, two] {
        assert!(
            state
                .replace("routines/title.yaml", source, &persisted_etag)
                .await
                .is_err()
        );
        assert!(Arc::ptr_eq(&active, &state.snapshot()));
    }
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
    assert_eq!(loaded.bundle.sources.len(), 16);
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

#[cfg(unix)]
#[tokio::test]
async fn watcher_matches_native_events_through_a_server_root_alias() {
    let directory = TestDirectory::new("watcher-real-root");
    let aliases = TestDirectory::new("watcher-root-alias");
    let alias = aliases.0.join("server");
    std::os::unix::fs::symlink(&directory.0, &alias).unwrap();
    let state = AgentDefinitionState::load(&alias, &[], "coding").unwrap();
    let watcher = state.spawn_watcher().unwrap();
    let replacement = agent("coding", "canonical event path");
    directory.resource("agents/coding.yaml", &replacement);
    wait_until(|| state.snapshot().sources()[0] == replacement).await;
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
