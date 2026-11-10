pub mod agent_definitions;
pub mod completion;
pub mod coordinator;
pub mod harness;
#[path = "../../../waltr-agent/tests/support/live_provider.rs"]
pub mod live_provider;
pub mod r#loop;
pub mod mcp;
pub mod permissions;
pub mod question;
pub mod sandbox;
#[path = "../../../waltr-agent/tests/support/scripted_provider.rs"]
pub mod scripted_provider;
pub mod session;
pub mod task;

pub async fn pending(
    broker: &crate::interaction::InteractionBroker,
    id: &waltr_agent::SessionId,
) -> crate::interaction::InteractionSnapshot {
    let mut subscription = broker.subscribe(id.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let snapshot = subscription.next().await.expect("interaction subscription");
            if !snapshot.pending.is_empty() {
                return snapshot;
            }
        }
    })
    .await
    .expect("interaction deadline")
}

pub fn respond(
    broker: &crate::interaction::InteractionBroker,
    snapshot: &crate::interaction::InteractionSnapshot,
    response: crate::interaction::InteractionResponse,
) -> Result<(), crate::interaction::InteractionError> {
    let item = &snapshot.pending[0];
    broker.respond(
        &waltr_agent::SessionId::new(item.session_id.clone()),
        &snapshot.generation,
        &item.id,
        &item.operation_id,
        response,
    )
}

pub fn fixture_binary() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("WALTR_TEST_MCP_SERVER") {
        return path.into();
    }
    let executable = std::env::current_exe().expect("test executable");
    executable
        .parent()
        .expect("test directory")
        .parent()
        .expect("profile directory")
        .join(format!(
            "waltr-server-test-mcp-server{}",
            std::env::consts::EXE_SUFFIX
        ))
}

pub mod live {
    pub mod coding_agent;
    pub mod context;
    pub mod filesystem;
    pub mod mcp;
    pub mod permissions;
    pub mod question;
    pub mod session_providers;
    #[cfg(feature = "shell")]
    pub mod shell;
    pub mod task;
}
