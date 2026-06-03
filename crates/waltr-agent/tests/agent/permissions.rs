//! Agent permission integration tests.

use std::{
    env, fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures_util::{StreamExt, stream};
use waltr_agent::{
    Agent, AgentEvent, BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest,
    LlmStream, ModelApi, ModelRef, PermissionDecision, PermissionEffect, PermissionOperation,
    PermissionPattern, PermissionPolicy, PermissionToolSelector, ProviderId, ToolCall,
    tools::filesystem::{FilesystemConfig, FilesystemToolProfile},
};
#[cfg(feature = "shell")]
use waltr_agent::{PermissionCommandPattern, PermissionResourcePattern, tools::shell::ShellConfig};
use serde_json::json;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
const DENIED_MESSAGE: &str = "external access denied by test";

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn filesystem_permissions_cover_default_allow_ask_allow_always_and_deny()
-> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
    let handler_calls = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "shell")]
    let shell_handler_calls = Arc::new(Mutex::new(Vec::new()));
    let handler = permission_handler(
        &fixture,
        Arc::clone(&handler_calls),
        #[cfg(feature = "shell")]
        Arc::clone(&shell_handler_calls),
    );
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
    let agent = agent.with_permission_handler(handler).build();
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
    let mut final_text = String::new();

    while let Some(event) = stream.next().await {
        match event? {
            AgentEvent::ToolResult { result, .. } => results.push(result.content),
            AgentEvent::TextDelta { delta, .. } => final_text.push_str(&delta),
            AgentEvent::ThinkingDelta { .. }
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
    assert!(
        results
            .iter()
            .any(|result| result.contains("external-once-202")),
        "{results:#?}"
    );
    assert!(
        results
            .iter()
            .any(|result| result.contains("external-nested-303")),
        "{results:#?}"
    );
    assert!(
        results
            .iter()
            .any(|result| result.contains("external-another-404")),
        "{results:#?}"
    );
    assert!(
        results.iter().any(|result| result.contains(DENIED_MESSAGE)),
        "{results:#?}"
    );
    #[cfg(feature = "shell")]
    {
        assert!(results.iter().any(|result| result.contains("shell-one")));
        assert!(results.iter().any(|result| result.contains("shell-two")));
        assert!(
            results
                .iter()
                .any(|result| result.contains("external-shell-walk-606")),
            "{results:#?}"
        );
        assert!(
            results
                .iter()
                .filter(|result| result.contains(DENIED_MESSAGE))
                .count()
                >= 2,
            "filesystem and shell denied path attempts should both be denied: {results:#?}"
        );
        assert!(
            !results
                .iter()
                .any(|result| result.contains("external-denied-505")),
            "denied shell path should not leak file contents: {results:#?}"
        );
        assert!(
            results
                .iter()
                .any(|result| result.contains("do not interact with git"))
        );
    }
    assert!(final_text.contains("permission-flow-complete"));

    let calls = handler_calls.lock().expect("handler calls").clone();
    #[cfg(not(feature = "shell"))]
    let expected_calls = 3;
    #[cfg(feature = "shell")]
    let expected_calls = 4;
    assert_eq!(
        calls.len(),
        expected_calls,
        "allow-always nested approval should be reused, while denied shell path still asks"
    );
    assert_eq!(calls[0], fixture.outside.path().join("external.txt"));
    assert_eq!(
        calls[1],
        fixture.outside.path().join("nested/external-nested.txt")
    );
    assert_eq!(calls[2], fixture.outside.path().join("denied.txt"));
    #[cfg(feature = "shell")]
    assert_eq!(calls[3], fixture.outside.path().join("denied.txt"));
    #[cfg(feature = "shell")]
    assert_eq!(
        shell_handler_calls
            .lock()
            .expect("shell handler calls")
            .as_slice(),
        &["printf shell-one".to_owned()],
        "allow-always shell approval should be reused"
    );

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
async fn multi_claim_denial_blocks_execution() -> waltr_agent::Result<()> {
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
    let agent = agent
        .with_permission_handler(move |_| PermissionDecision::Deny(DENIED_MESSAGE.to_owned()))
        .build();
    let provider = PatchDeniedProvider {
        allowed: fixture.root.path().join("allowed.txt"),
        denied,
        #[cfg(feature = "shell")]
        shell_denied: shell_denied.clone(),
    };

    let mut stream = agent.prompt(&provider, "patch denied").stream();
    let mut result = String::new();
    while let Some(event) = stream.next().await {
        if let AgentEvent::ToolResult {
            result: tool_result,
            ..
        } = event?
        {
            result = tool_result.content;
        }
    }

    assert!(result.contains(DENIED_MESSAGE));
    assert_eq!(fixture.root.read("allowed.txt")?, "root-allowed-101\n");
    #[cfg(feature = "shell")]
    assert!(!shell_denied.exists());

    Ok(())
}

fn permission_handler(
    fixture: &PermissionFixture,
    calls: Arc<Mutex<Vec<PathBuf>>>,
    #[cfg(feature = "shell")] shell_calls: Arc<Mutex<Vec<String>>>,
) -> impl Fn(&waltr_agent::PermissionRequest) -> PermissionDecision + Send + Sync + 'static {
    let external = fixture.outside.path().join("external.txt");
    let nested_file = fixture.outside.path().join("nested/external-nested.txt");
    let nested_dir = fixture.outside.path().join("nested");
    move |request| {
        #[cfg(feature = "shell")]
        if let Some(command) = shell_claim_command(request) {
            shell_calls
                .lock()
                .expect("shell handler calls")
                .push(command);
            return PermissionDecision::AllowAlways(vec![PermissionPattern {
                operation: Some(PermissionOperation::Execute),
                resource: PermissionResourcePattern::ShellCommandPrefix("printf ".to_owned()),
            }]);
        }

        let path = filesystem_claim_path(request).expect("filesystem path claim");
        calls.lock().expect("handler calls").push(path.clone());
        if path == external {
            PermissionDecision::AllowOnce
        } else if path == nested_file {
            PermissionDecision::AllowAlways(vec![PermissionPattern::filesystem_prefix(
                PermissionOperation::Read,
                nested_dir.clone(),
            )])
        } else {
            PermissionDecision::Deny(DENIED_MESSAGE.to_owned())
        }
    }
}

#[cfg(feature = "shell")]
fn shell_claim_command(request: &waltr_agent::PermissionRequest) -> Option<String> {
    request
        .claims
        .iter()
        .find_map(|claim| match &claim.resource {
            waltr_agent::PermissionResource::ShellCommand(command) => Some(command.clone()),
            waltr_agent::PermissionResource::FilesystemPath(_)
            | waltr_agent::PermissionResource::FilesystemPathPrefix(_)
            | waltr_agent::PermissionResource::WorkingDirectory(_) => None,
        })
}

fn filesystem_claim_path(request: &waltr_agent::PermissionRequest) -> Option<PathBuf> {
    request
        .claims
        .iter()
        .find_map(|claim| match &claim.resource {
            waltr_agent::PermissionResource::FilesystemPath(path)
            | waltr_agent::PermissionResource::FilesystemPathPrefix(path) => Some(path.clone()),
            waltr_agent::PermissionResource::WorkingDirectory(_)
            | waltr_agent::PermissionResource::ShellCommand(_) => None,
        })
}

struct ScriptedPermissionProvider;

impl LlmStream for ScriptedPermissionProvider {
    fn name(&self) -> &'static str {
        "scripted_permission_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let step = request.tool_results.len();
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
        Box::pin(async move {
            let events = match request.tool_results.len() {
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
