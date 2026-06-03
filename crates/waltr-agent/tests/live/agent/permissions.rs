//! Live provider agent permission integration tests.

use std::{
    env, fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use waltr_agent::{
    Agent, LlmStream, ModelRef, PermissionDecision, PermissionEffect, PermissionOperation,
    PermissionPattern, PermissionPolicy,
    tools::filesystem::{FilesystemConfig, FilesystemToolProfile},
};
#[cfg(feature = "shell")]
use waltr_agent::{
    PermissionCommandPattern, PermissionResourcePattern, PermissionToolSelector,
    tools::shell::ShellConfig,
};

use crate::{live_provider, session::AgentTestSession};

const DENIED_MESSAGE: &str = "external access denied by test";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_permissions_agent_loop() -> waltr_agent::Result<()> {
    let live =
        live_provider::selected_from_env("WALTR_AGENT_LIVE_PROVIDER", "openai_responses_websocket")?;
    run_live_permissions(live.provider, live.model).await
}

#[allow(clippy::too_many_lines)]
async fn run_live_permissions(provider: impl LlmStream, model: ModelRef) -> waltr_agent::Result<()> {
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
            PermissionCommandPattern::Any,
            PermissionEffect::allow(),
        )
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("git".to_owned()),
            PermissionEffect::deny_with_message("do not interact with git"),
        );
    let agent = Agent::builder(model)
        .with_permission_policy(policy)
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_profile(FilesystemToolProfile::StructuredPatch),
        );
    #[cfg(feature = "shell")]
    let agent = agent.with_shell_tool(ShellConfig::new(fixture.root.path()));
    let agent = agent.with_permission_handler(handler).build();

    #[cfg(not(feature = "shell"))]
    let prompt = format!(
        "Use read exactly five times. Read allowed.txt in the root. Read this external file: {}. Read these two external nested files: {} and {}. Then attempt this denied external file: {}. Report a concise final status containing root-allowed-101, external-once-202, external-nested-303, external-another-404, and the exact denial message {DENIED_MESSAGE}.",
        fixture.outside.path().join("external.txt").display(),
        fixture
            .outside
            .path()
            .join("nested/external-nested.txt")
            .display(),
        fixture.outside.path().join("nested/another.txt").display(),
        fixture.outside.path().join("denied.txt").display(),
    );
    #[cfg(feature = "shell")]
    let prompt = format!(
        "Use read exactly five times. Read allowed.txt in the root. Read this external file: {}. Read these two external nested files: {} and {}. Then attempt this denied external file: {}. Also use the shell tool to print exactly waltr-shell-ok with printf. Use the shell tool with cat to read this external nested file covered by the already approved nested directory: {}. Then use the shell tool with cat to attempt this denied external file: {}. Finally attempt git status so the tool reports the exact denial message do not interact with git. Report a concise final status containing root-allowed-101, external-once-202, external-nested-303, external-another-404, external-shell-walk-606, waltr-shell-ok, {DENIED_MESSAGE}, and do not interact with git.",
        fixture.outside.path().join("external.txt").display(),
        fixture
            .outside
            .path()
            .join("nested/external-nested.txt")
            .display(),
        fixture.outside.path().join("nested/another.txt").display(),
        fixture.outside.path().join("denied.txt").display(),
        fixture
            .outside
            .path()
            .join("nested/shell-walk.txt")
            .display(),
        fixture.outside.path().join("denied.txt").display(),
    );
    let session = AgentTestSession::run(&agent, &provider, &prompt).await?;
    session.require_tool_execution(provider.name())?;
    crate::session::report_tool_errors(&provider, &session, "permissions live test");
    let context = crate::session::session_context(&provider, &session);

    assert!(session.text.contains("root-allowed-101"), "{context}");
    assert!(session.text.contains("external-once-202"), "{context}");
    assert!(session.text.contains("external-nested-303"), "{context}");
    assert!(session.text.contains("external-another-404"), "{context}");
    assert!(session.text.contains(DENIED_MESSAGE), "{context}");
    #[cfg(feature = "shell")]
    {
        assert!(session.text.contains("waltr-shell-ok"), "{context}");
        assert!(
            session.text.contains("external-shell-walk-606"),
            "{context}"
        );
        assert!(
            session.text.contains("do not interact with git"),
            "{context}"
        );
    }
    #[cfg(not(feature = "shell"))]
    let expected_handler_calls = 3;
    #[cfg(feature = "shell")]
    let expected_handler_calls = 4;
    assert_eq!(
        handler_calls.lock().expect("handler calls").len(),
        expected_handler_calls,
        "{context}"
    );

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
            "waltr-agent-permissions-live-test-{}-{}",
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
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}
