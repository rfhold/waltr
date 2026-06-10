//! Agent permission integration tests.

use std::{env, fs, path::PathBuf, sync::Mutex};

use futures_util::{StreamExt, stream};
use waltr_agent::{
    Agent, AgentEvent, BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest,
    LlmStream, ModelApi, ModelRef, PermissionEffect, PermissionPolicy, PermissionToolSelector,
    ProviderId, ToolCall,
    middleware::ExternalRequestKind,
    tools::filesystem::{FilesystemConfig, FilesystemToolProfile},
};
#[cfg(feature = "shell")]
use waltr_agent::{PermissionCommandPattern, tools::shell::ShellConfig};
use serde_json::json;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
#[tokio::test]
#[allow(clippy::too_many_lines)]
// TODO(agent-runtime): Add coverage that permission prompts survive durable suspension/resume.
async fn filesystem_permissions_allow_root_and_suspend_for_external_access()
-> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let policy =
        PermissionPolicy::filesystem_external(fixture.root.path(), PermissionEffect::ask());
    #[cfg(feature = "shell")]
    let policy = policy
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("printf".to_owned()),
            PermissionEffect::ask(),
        )
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("cat".to_owned()),
            PermissionEffect::allow(),
        )
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("git".to_owned()),
            PermissionEffect::deny_with_message("do not interact with git"),
        );
    let agent = Agent::builder(test_model(ModelApi::OpenAiResponses))
        .with_permission_policy(policy)
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        );
    #[cfg(feature = "shell")]
    let agent = agent.with_shell_tool(ShellConfig::new(fixture.root.path()));
    let agent = agent.build();
    let provider = ScriptedPermissionProvider;
    #[cfg(not(feature = "shell"))]
    let prompt = "exercise filesystem permission allow, ask, allow-always, and deny".to_owned();
    #[cfg(feature = "shell")]
    let prompt = format!(
        "exercise filesystem permissions, use shell printf twice, use shell cat to read {}, use shell cat to attempt denied {}, then attempt git status",
        fixture
            .outside
            .path()
            .join("nested/shell-walk.txt")
            .display(),
        fixture.outside.path().join("denied.txt").display(),
    );

    let mut stream = agent.prompt(&provider, &prompt).stream();
    let mut results = Vec::new();
    let mut external_requests = Vec::new();

    while let Some(event) = stream.next().await {
        match event? {
            AgentEvent::ToolResult { result, .. } => results.push(result.content),
            AgentEvent::ExternalRequest { request, .. } => external_requests.push(request),
            AgentEvent::ThinkingDelta { .. }
            | AgentEvent::TextDelta { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::TurnFinished { .. }
            | AgentEvent::Finished { .. } => {}
        }
    }

    assert!(
        results
            .iter()
            .any(|result| result.contains("root-allowed-101")),
        "{results:#?}"
    );
    let [request] = external_requests.as_slice() else {
        panic!("expected one permission external request: {external_requests:#?}");
    };
    assert_eq!(request.kind, ExternalRequestKind::Permission);
    assert_eq!(request.tool_call_id.as_deref(), Some("call_2"));
    assert!(request.prompt.contains("external.txt"), "{request:#?}");
    assert!(request.id.starts_with("permission:ephemeral:1:call_2"));

    Ok(())
}

#[tokio::test]
async fn blanket_denied_tools_are_not_surfaced() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let policy = PermissionPolicy::filesystem_default(fixture.root.path())
        .with_tool_rule(
            PermissionToolSelector::exact("edit"),
            PermissionEffect::deny(),
        )
        .with_tool_rule(
            PermissionToolSelector::exact("write"),
            PermissionEffect::deny(),
        )
        .with_tool_rule(
            PermissionToolSelector::exact("delete"),
            PermissionEffect::deny(),
        )
        .with_tool_rule(
            PermissionToolSelector::exact("apply_patch"),
            PermissionEffect::deny(),
        );
    #[cfg(feature = "shell")]
    let policy = policy.with_tool_rule(
        PermissionToolSelector::exact("shell"),
        PermissionEffect::deny(),
    );
    let agent = Agent::builder(test_model(ModelApi::OpenAiResponses))
        .with_permission_policy(policy)
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        );
    #[cfg(feature = "shell")]
    let agent = agent.with_shell_tool(ShellConfig::new(fixture.root.path()));
    let agent = agent.build();
    let provider = CaptureToolsProvider::default();

    let mut stream = agent.prompt(&provider, "capture tools").stream();
    while let Some(event) = stream.next().await {
        drop(event?);
    }

    let tools = provider.tools.lock().expect("captured tools").clone();
    assert!(tools.iter().any(|name| name == "read"));
    assert!(tools.iter().any(|name| name == "glob"));
    assert!(tools.iter().any(|name| name == "grep"));
    assert!(!tools.iter().any(|name| name == "edit"));
    assert!(!tools.iter().any(|name| name == "write"));
    assert!(!tools.iter().any(|name| name == "delete"));
    assert!(!tools.iter().any(|name| name == "apply_patch"));
    #[cfg(feature = "shell")]
    assert!(!tools.iter().any(|name| name == "shell"));

    Ok(())
}

#[tokio::test]
async fn multi_claim_ask_suspends_before_execution() -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let denied = fixture.outside.path().join("denied.txt");
    #[cfg(feature = "shell")]
    let shell_denied = fixture.outside.path().join("shell-denied.txt");
    let agent = Agent::builder(test_model(ModelApi::OpenAiResponses))
        .with_permission_policy(PermissionPolicy::filesystem_external(
            fixture.root.path(),
            PermissionEffect::ask(),
        ))
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        );
    #[cfg(feature = "shell")]
    let agent = agent.with_shell_tool(ShellConfig::new(fixture.root.path()));
    let agent = agent.build();
    let provider = PatchDeniedProvider {
        allowed: fixture.root.path().join("allowed.txt"),
        denied,
        #[cfg(feature = "shell")]
        shell_denied: shell_denied.clone(),
    };

    let mut stream = agent.prompt(&provider, "patch denied").stream();
    let mut external_requests = Vec::new();
    while let Some(event) = stream.next().await {
        if let AgentEvent::ExternalRequest { request, .. } = event? {
            external_requests.push(request);
        }
    }

    let [request] = external_requests.as_slice() else {
        panic!("expected one permission external request: {external_requests:#?}");
    };
    assert_eq!(request.kind, ExternalRequestKind::Permission);
    assert_eq!(request.tool_call_id.as_deref(), Some("call_1"));
    assert!(
        request.prompt.contains("Write filesystem path"),
        "{request:#?}"
    );
    assert_eq!(fixture.root.read("allowed.txt")?, "root-allowed-101\n");
    #[cfg(feature = "shell")]
    assert!(!shell_denied.exists());

    Ok(())
}

struct ScriptedPermissionProvider;

impl LlmStream for ScriptedPermissionProvider {
    fn name(&self) -> &'static str {
        "scripted_permission_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        // TODO(agent-runtime): Drive scripted turns from request records, not tool_result counts.
        Box::pin(async move {
            let step = request.tool_results().count();
            let events = match step {
                0 => vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_1",
                    "allowed.txt",
                )))],
                1 => vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_2",
                    "../outside/external.txt",
                )))],
                2 => vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_3",
                    "../outside/nested/external-nested.txt",
                )))],
                3 => vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_4",
                    "../outside/nested/another.txt",
                )))],
                4 => vec![Ok(CompletionEvent::ToolCall(read_call(
                    "call_5",
                    "../outside/denied.txt",
                )))],
                #[cfg(feature = "shell")]
                5 => vec![Ok(CompletionEvent::ToolCall(shell_call(
                    "call_6",
                    "printf shell-one",
                    "first shell command",
                )))],
                #[cfg(feature = "shell")]
                6 => vec![Ok(CompletionEvent::ToolCall(shell_call(
                    "call_7",
                    "printf shell-two",
                    "second shell command",
                )))],
                #[cfg(feature = "shell")]
                7 => vec![Ok(CompletionEvent::ToolCall(shell_call(
                    "call_8",
                    "cat ../outside/nested/shell-walk.txt",
                    "read a shell path covered by the approved nested directory",
                )))],
                #[cfg(feature = "shell")]
                8 => vec![Ok(CompletionEvent::ToolCall(shell_call(
                    "call_9",
                    "cat ../outside/denied.txt",
                    "read a shell path denied by external filesystem policy",
                )))],
                #[cfg(feature = "shell")]
                9 => vec![Ok(CompletionEvent::ToolCall(shell_call(
                    "call_10",
                    "git status",
                    "denied shell command",
                )))],
                _ => vec![
                    Ok(CompletionEvent::Delta(
                        "permission-flow-complete".to_owned(),
                    )),
                    Ok(CompletionEvent::Finished),
                ],
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

struct CaptureToolsProvider {
    tools: Mutex<Vec<String>>,
}

impl Default for CaptureToolsProvider {
    fn default() -> Self {
        Self {
            tools: Mutex::new(Vec::new()),
        }
    }
}

impl LlmStream for CaptureToolsProvider {
    fn name(&self) -> &'static str {
        "capture_tools_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            *self.tools.lock().expect("captured tools") =
                request.tools.iter().map(|tool| tool.name.clone()).collect();
            Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)])) as BoxCompletionStream)
        })
    }
}

struct PatchDeniedProvider {
    allowed: PathBuf,
    denied: PathBuf,
    #[cfg(feature = "shell")]
    shell_denied: PathBuf,
}

impl LlmStream for PatchDeniedProvider {
    fn name(&self) -> &'static str {
        "patch_denied_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        // TODO(agent-runtime): Verify denied multi-claim work records are canceled durably.
        Box::pin(async move {
            let events = match request.tool_results().count() {
                0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "call_1",
                    "apply_patch",
                    json!({
                        "patchText": format!(
                            "*** Begin Patch\n*** Update File: {}\n+mutated\n*** Update File: {}\n+mutated\n*** End Patch",
                            self.allowed.display(),
                            self.denied.display()
                        )
                    }),
                )))],
                #[cfg(feature = "shell")]
                1 => vec![Ok(CompletionEvent::ToolCall(shell_call(
                    "call_2",
                    &format!("printf mutated > {}", self.shell_denied.display()),
                    "test path denial",
                )))],
                _ => vec![Ok(CompletionEvent::Finished)],
            };
            Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
        })
    }
}

fn read_call(id: &str, file_path: &str) -> ToolCall {
    ToolCall::new(id, "read", json!({ "filePath": file_path }))
}

#[cfg(feature = "shell")]
fn shell_call(id: &str, command: &str, description: &str) -> ToolCall {
    ToolCall::new(
        id,
        "shell",
        json!({ "command": command, "description": description }),
    )
}

fn test_model(api: ModelApi) -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, OPENAI_TEST_MODEL, api)
}

struct PermissionFixture {
    _parent: TempRoot,
    root: TempRoot,
    outside: TempRoot,
}

impl PermissionFixture {
    fn new() -> waltr_agent::Result<Self> {
        let parent = TempRoot::new()?;
        let root = TempRoot::at(parent.path().join("root"))?;
        let outside = TempRoot::at(parent.path().join("outside"))?;
        root.write("allowed.txt", "root-allowed-101\n")?;
        root.write("nested/inside.txt", "root-nested-inside\n")?;
        outside.write("external.txt", "external-once-202\n")?;
        outside.write("nested/external-nested.txt", "external-nested-303\n")?;
        outside.write("nested/another.txt", "external-another-404\n")?;
        outside.write("nested/shell-walk.txt", "external-shell-walk-606\n")?;
        outside.write("denied.txt", "external-denied-505\n")?;
        Ok(Self {
            _parent: parent,
            root,
            outside,
        })
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> waltr_agent::Result<Self> {
        Self::at(env::temp_dir().join(format!(
            "waltr-agent-permissions-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        )))
    }

    fn at(path: PathBuf) -> waltr_agent::Result<Self> {
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    fn write(&self, path: &str, content: &str) -> waltr_agent::Result<()> {
        let path = self.path.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(fs::write(path, content)?)
    }

    fn read(&self, path: &str) -> waltr_agent::Result<String> {
        Ok(fs::read_to_string(self.path.join(path))?)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}
