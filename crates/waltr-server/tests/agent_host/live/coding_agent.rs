use crate::host_tests::{
    harness::HostSession, live_provider, sandbox::TestSandbox,
    scripted_provider::CapturingProvider, session::AgentTestSession,
};
use crate::{
    agent_host::{
        HostRuntimeBuilder, SkillsConfig,
        tools::{
            filesystem::{FilesystemConfig, FilesystemToolProfile},
            question::QuestionConfig,
        },
    },
    interaction::{Interaction, InteractionResponse},
    permission::*,
};
use std::sync::Arc;
use waltr_agent::*;

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_coding_agent_context_skills_permissions_and_questions_keep_original_owner()
-> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_responses_websocket",
    ))?
    else {
        return Ok(());
    };
    let parent = tempfile::tempdir()?;
    let root = parent.path().join("root");
    let outside = parent.path().join("outside");
    for (path, content) in [
        (
            "AGENTS.md",
            "Root instruction token: root-agent-101. Include root-agent-101 in observed output.\n",
        ),
        (
            "project/AGENTS.md",
            "Project instruction token: project-agent-202. Include project-agent-202 in observed output. Read docs/project-reference.md and include project-reference-303 in observed output.\n",
        ),
        (
            "project/src/task.txt",
            "Task token: task-token-404. Include task-token-404 in observed output.\n",
        ),
        (
            "project/docs/project-reference.md",
            "Project reference token: project-reference-303.\n",
        ),
        (
            "project/.agents/skills/coding-style/SKILL.md",
            "---\nname: coding-style\ndescription: Apply the repository coding style.\n---\nSkill token: style-skill-505. Include style-skill-505 in observed output. Read references/style.md and include style-reference-606 in observed output.\n",
        ),
        (
            "project/.agents/skills/coding-style/references/style.md",
            "Style reference token: style-reference-606.\n",
        ),
    ] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, content)?;
    }
    std::fs::create_dir_all(&outside)?;
    std::fs::write(outside.join("external.txt"), "external-permission-707\n")?;
    let sandbox = TestSandbox::new(&root).await;
    let provider = Arc::new(CapturingProvider::new(live.provider));
    let host = HostSession::new(
        HostRuntimeBuilder::new(live.model, provider.clone())
            .with_sandbox(sandbox.handle())
            .with_system_prompt("You are a coding agent validating durable middleware execution.")
            .with_permission_policy(PermissionPolicy::filesystem_external(
                &root,
                PermissionEffect::ask(),
            ))
            .with_filesystem_tools(
                FilesystemConfig::new(&root)
                    .with_profile(FilesystemToolProfile::StructuredPatch)
                    .with_agents_read_context()
                    .with_skills_context(),
            )
            .with_skills(SkillsConfig::new(&root))
            .with_question_tool(QuestionConfig::new()),
    )
    .await;
    let owner = host.start(format!("First use question exactly once to ask whether to proceed. Do not read files before the answer. After proceeding read project/src/task.txt, follow its AGENTS.md instructions, load coding-style, and read the required skill and project references. Then attempt to read this external file exactly once: {}. Report exact observed tokens only.",outside.join("external.txt").display()));
    let question = crate::host_tests::pending(&host.broker, &host.id).await;
    let Interaction::Question { input } = &question.pending[0].request else {
        panic!("question")
    };
    let answers = input
        .questions
        .iter()
        .map(|q| {
            vec![
                q.options
                    .first()
                    .map_or_else(|| "Proceed".into(), |o| o.label.clone()),
            ]
        })
        .collect();
    assert!(!owner.is_finished());
    crate::host_tests::respond(
        &host.broker,
        &question,
        InteractionResponse::Question(answers),
    )
    .unwrap();
    let permission = crate::host_tests::pending(&host.broker, &host.id).await;
    assert!(matches!(
        permission.pending[0].request,
        Interaction::Permission { .. }
    ));
    assert!(!owner.is_finished());
    let record = host
        .sessions
        .load(&host.id)
        .await
        .map_err(|e| LlmError::Protocol {
            provider: "coding-test",
            message: e.to_string(),
        })?
        .record;
    let snapshot = record.snapshot();
    assert!(snapshot.tool_results().any(|r| r.name == "question"));
    let observed = snapshot
        .tool_results()
        .map(|r| r.content.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    for token in [
        "root-agent-101",
        "project-agent-202",
        "task-token-404",
        "style-skill-505",
        "style-reference-606",
        "project-reference-303",
    ] {
        assert!(observed.contains(token), "missing {token}: {observed}");
    }
    crate::host_tests::respond(
        &host.broker,
        &permission,
        InteractionResponse::Permission(PermissionResponse::AllowOnce),
    )
    .unwrap();
    let result = AgentTestSession::from_events(owner.await.unwrap())?;
    assert!(result.finished);
    for name in ["question", "read", "load_skill"] {
        assert!(provider.all_tool_names().iter().any(|n| n == name));
    }
    assert!(host.broker.snapshot(&host.id).pending.is_empty());
    host.close().await;
    Ok(())
}
