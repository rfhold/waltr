//! Live provider agent filesystem integration tests.

use std::{env, fs, path::PathBuf};

use waltr_agent::{
    Agent, LlmStream, ModelRef,
    tools::filesystem::{FilesystemConfig, FilesystemToolProfile},
};

use crate::{
    live_provider,
    session::{self, AgentTestSession},
};

const LOOKUP_KEY: &str = "waltr_status_value";
const ORIGINAL_VALUE: &str = "filesystem-agent-original-1842";
const UPDATED_VALUE: &str = "filesystem-agent-updated-7391";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_runs_filesystem_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::anthropic_messages())?
    else {
        return Ok(());
    };
    assert_provider_runs_filesystem_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_runs_filesystem_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions())?
    else {
        return Ok(());
    };
    assert_provider_runs_filesystem_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_runs_filesystem_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::openai_responses())?
    else {
        return Ok(());
    };
    assert_provider_runs_filesystem_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_runs_filesystem_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_websocket())?
    else {
        return Ok(());
    };
    assert_provider_runs_filesystem_agent(live.provider, live.model).await
}

async fn assert_provider_runs_filesystem_agent(
    provider: impl LlmStream,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let root = TempRoot::new()?;
    root.write("alpha.txt", "alpha=one\n")?;
    root.write(
        "status.txt",
        &format!("name=waltr\n{LOOKUP_KEY}={ORIGINAL_VALUE}\n"),
    )?;
    root.write("delete-me.txt", "remove this file\n")?;
    root.write("notes/nested.txt", "nested=present\n")?;

    let agent = Agent::builder(model)
        .with_filesystem_tools(
            FilesystemConfig::new(root.path()).with_profile(FilesystemToolProfile::StructuredPatch),
        )
        .build();

    let report = run_agent_session(
        &agent,
        &provider,
        &format!(
            "Use filesystem tools to list the existing files with glob pattern **/*, then use the grep tool with pattern exactly {LOOKUP_KEY} to search all files. Report the existing files and the matching value {ORIGINAL_VALUE}. Keep the response concise."
        ),
    )
    .await?;
    session::report_tool_errors(&provider, &report, "filesystem report turn");
    let report_context = session::session_context(&provider, &report);
    assert!(report.text.contains(ORIGINAL_VALUE), "{report_context}");
    assert!(report.text.contains("alpha.txt"), "{report_context}");
    assert!(report.text.contains("status.txt"), "{report_context}");
    assert!(report.text.contains("delete-me.txt"), "{report_context}");
    assert!(
        report.text.contains("nested.txt") || report.text.contains("notes/"),
        "{report_context}"
    );

    let mutation = run_agent_session(
        &agent,
        &provider,
        &format!(
            "In one turn, use filesystem tools to make exactly these changes: replace {ORIGINAL_VALUE} with {UPDATED_VALUE} in status.txt, delete delete-me.txt, and create created.txt containing exactly created_by=agent\\n. Then answer with only done."
        ),
    )
    .await?;
    session::report_tool_errors(&provider, &mutation, "filesystem mutation turn");
    let mutation_context = session::session_context(&provider, &mutation);

    assert!(
        root.read("status.txt")?.contains(UPDATED_VALUE),
        "{mutation_context}"
    );
    assert!(
        !root.path().join("delete-me.txt").exists(),
        "{mutation_context}"
    );
    assert_eq!(
        root.read("created.txt")?
            .trim_end_matches(['\\', 'n', '\n']),
        "created_by=agent",
        "{mutation_context}"
    );

    Ok(())
}

async fn run_agent_session(
    agent: &Agent,
    provider: &impl LlmStream,
    prompt: &str,
) -> waltr_agent::Result<AgentTestSession> {
    let session = AgentTestSession::run(agent, provider, prompt).await?;
    session.require_tool_execution(provider.name())?;
    Ok(session)
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> waltr_agent::Result<Self> {
        let path = env::temp_dir().join(format!(
            "waltr-agent-filesystem-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
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
