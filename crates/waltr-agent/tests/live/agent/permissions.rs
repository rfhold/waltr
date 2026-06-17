//! Live provider agent permission integration tests.

use std::{env, fs, path::PathBuf};

use waltr_agent::{
    Agent, ExternalRequestKind, LlmStream, ModelRef, PermissionEffect, PermissionPolicy,
    tools::filesystem::{FilesystemConfig, FilesystemToolProfile},
};
#[cfg(feature = "shell")]
use waltr_agent::{PermissionCommandPattern, PermissionToolSelector, tools::shell::ShellConfig};

use crate::{live_provider, session::AgentTestSession};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_permissions_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_responses_websocket",
    ))?
    else {
        return Ok(());
    };
    run_live_permissions(live.provider, live.model).await
}

#[allow(clippy::too_many_lines)]
async fn run_live_permissions(provider: impl LlmStream, model: ModelRef) -> waltr_agent::Result<()> {
    let fixture = PermissionFixture::new()?;
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
    let agent = agent.build();

    #[cfg(not(feature = "shell"))]
    let prompt = format!(
        "Use read exactly twice. Read allowed.txt in the root. Then read this external file: {}.",
        fixture.outside.path().join("external.txt").display(),
    );
    #[cfg(feature = "shell")]
    let prompt = format!(
        "Use read exactly twice. Read allowed.txt in the root. Then read this external file: {}.",
        fixture.outside.path().join("external.txt").display(),
    );
    let session = AgentTestSession::run(&agent, &provider, &prompt).await?;
    crate::session::report_tool_errors(&provider, &session, "permissions live test");
    let context = crate::session::session_context(&provider, &session);

    assert!(
        session
            .external_requests
            .iter()
            .any(|request| request.kind == ExternalRequestKind::Permission),
        "{context}"
    );

    Ok(())
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
