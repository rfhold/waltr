//! Agent context-loading integration tests.

use std::{env, fs, path::PathBuf};

use waltr_agent::{
    Agent, LlmStream, ModelRef,
    tools::{filesystem::FilesystemConfig, filesystem_skills::SkillsConfig},
};

use crate::{live_provider, session::AgentTestSession};

const COMPLETION_MARKER: &str = "dynamic-context-flow-complete";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_context_agent_loop() -> waltr_agent::Result<()> {
    let live =
        live_provider::selected_from_env("WALTR_AGENT_LIVE_PROVIDER", "openai_responses_websocket")?;
    run_live_context(live.provider, live.model).await
}

async fn run_live_context(provider: impl LlmStream, model: ModelRef) -> waltr_agent::Result<()> {
    let fixture = ContextFixture::new()?;
    let agent = Agent::builder(model)
        .with_system_prompt("You are a coding agent validating dynamic context loading.")
        .with_filesystem_tools(
            FilesystemConfig::new(fixture.root.path())
                .with_agents_read_context()
                .with_skills_context(),
        )
        .with_skills(SkillsConfig::new(fixture.root.path()))
        .build();
    let prompt = "Use the read tool to read project/src/task.txt. Follow the dynamically loaded AGENTS.md instructions from that read. When the read result reveals the coding-style skill, use load_skill for coding-style before following its instructions. Read the skill reference file requested by the loaded skill. Read the project reference file requested by AGENTS.md. Report a concise final status containing every exact token the task, AGENTS.md files, loaded skill, and referenced files ask you to include.";

    let session = AgentTestSession::run(&agent, &provider, prompt).await?;
    session.require_tool_execution(provider.name())?;
    crate::session::report_tool_errors(&provider, &session, "context live test");
    let context = crate::session::session_context(&provider, &session);

    assert!(session.text.contains(COMPLETION_MARKER), "{context}");
    assert!(session.text.contains("root-agent-101"), "{context}");
    assert!(session.text.contains("project-agent-202"), "{context}");
    assert!(session.text.contains("task-token-404"), "{context}");
    assert!(session.text.contains("style-skill-505"), "{context}");
    assert!(session.text.contains("style-reference-606"), "{context}");
    assert!(session.text.contains("project-reference-303"), "{context}");

    Ok(())
}

struct ContextFixture {
    root: TempRoot,
}

impl ContextFixture {
    fn new() -> waltr_agent::Result<Self> {
        let root = TempRoot::new()?;
        root.write(
            "AGENTS.md",
            "Root instruction token: root-agent-101. Include root-agent-101 in the final status.\n",
        )?;
        root.write(
            "project/AGENTS.md",
            "Project instruction token: project-agent-202. Include project-agent-202 in the final status. Read docs/project-reference.md before finishing and include project-reference-303 in the final status.\n",
        )?;
        root.write(
            "project/src/task.txt",
            "Task token: task-token-404. Include task-token-404 and dynamic-context-flow-complete in the final status.\n",
        )?;
        root.write(
            "project/docs/project-reference.md",
            "Project reference token: project-reference-303. Include project-reference-303 in the final status.\n",
        )?;
        root.write(
            "project/.agents/skills/coding-style/SKILL.md",
            r"---
name: coding-style
description: Apply the repository coding style.
---
Skill token: style-skill-505. Include style-skill-505 in the final status.
Read references/style.md before finishing and include style-reference-606 in the final status.
",
        )?;
        root.write(
            "project/.agents/skills/coding-style/references/style.md",
            "Style reference token: style-reference-606. Include style-reference-606 in the final status.\n",
        )?;
        Ok(Self { root })
    }
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> waltr_agent::Result<Self> {
        Self::at(env::temp_dir().join(format!(
            "waltr-agent-context-test-{}-{}",
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
