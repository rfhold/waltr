use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use mcp::{
    McpCompletionRequest, McpProgressNotification, McpRoot, McpToolCall,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::standard_surface::{
    ECHO_TOOL, GREET_PROMPT, ITEM_42, ITEM_TEMPLATE, LOG_TOOL, PROGRESS_TOOL, STATUS_PROMPT,
    STATUS_RESOURCE,
};

pub async fn assert_standard_surface(
    case_name: &str,
    transport: McpTransportConfig,
) -> mcp::Result<()> {
    let handler = Arc::new(TestClientHandler::default());
    let client = McpClient::connect(
        case_name.to_owned(),
        transport,
        handler.clone(),
        McpClientOptions::new("mcp-ecosystem-test", "0", Duration::from_secs(5))
            .with_log_level(mcp::McpLogLevel::Info),
        CancellationToken::new(),
    )
    .await?;
    assert_surface(&client).await?;

    let has_log = handler
        .logs
        .lock()
        .expect("logs lock")
        .iter()
        .any(|log| log.to_string().contains("ecosystem-log-notification-818"));
    assert!(has_log);

    let has_progress = handler
        .progress
        .lock()
        .expect("progress lock")
        .iter()
        .any(|item| item.to_string().contains("ecosystem-progress-marker"));
    assert!(has_progress);

    client.shutdown().await?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn assert_surface(client: &McpClient) -> mcp::Result<()> {
    let cancel = CancellationToken::new();

    let tools = client.list_tools(cancel.clone()).await?;
    assert!(tools.iter().any(|tool| tool.name == ECHO_TOOL));
    assert!(tools.iter().any(|tool| tool.name == LOG_TOOL));
    assert!(tools.iter().any(|tool| tool.name == PROGRESS_TOOL));

    let echo = client
        .call_tool(
            McpToolCall::new(ECHO_TOOL, json!({ "value": "ecosystem-tool-marker-707" })),
            cancel.clone(),
        )
        .await?;
    assert!(echo.format_text().contains("ecosystem-tool-marker-707"));

    let log = client
        .call_tool(
            McpToolCall::new(LOG_TOOL, json!({ "message": "ecosystem-log-marker-818" })),
            cancel.clone(),
        )
        .await?;
    assert!(log.format_text().contains("ecosystem-log-marker-818"));

    let progress = client
        .call_tool(
            McpToolCall::new(
                PROGRESS_TOOL,
                json!({ "value": "ecosystem-progress-result-919" }),
            )
            .with_progress_token("ecosystem-progress-token"),
            cancel.clone(),
        )
        .await?;
    assert!(
        progress
            .format_text()
            .contains("ecosystem-progress-result-919")
    );

    let prompts = client.list_prompts(cancel.clone()).await?;
    assert!(prompts.iter().any(|prompt| prompt.name == STATUS_PROMPT));
    assert!(prompts.iter().any(|prompt| prompt.name == GREET_PROMPT));

    let prompt = client
        .get_prompt(
            GREET_PROMPT,
            Some(json!({ "name": "ecosystem" })),
            cancel.clone(),
        )
        .await?;
    assert!(
        prompt
            .format_text()
            .contains("hello ecosystem from ecosystem-prompt-414")
    );

    let resources = client.list_resources(cancel.clone()).await?;
    assert!(
        resources
            .iter()
            .any(|resource| resource.name == "status_resource")
    );

    let templates = client.list_resource_templates(cancel.clone()).await?;
    assert!(
        templates
            .iter()
            .any(|template| template.name == "item_resource")
    );

    let resource = client
        .read_resource(STATUS_RESOURCE, cancel.clone())
        .await?;
    assert!(
        resource
            .format_text()
            .contains("ecosystem-resource-status-616")
    );

    let item_resource = client.read_resource(ITEM_42, cancel.clone()).await?;
    assert!(
        item_resource
            .format_text()
            .contains("ecosystem-resource-template-item-42-717")
    );

    let completion = client
        .complete_resource_template(
            McpCompletionRequest::new(ITEM_TEMPLATE, "id", "42"),
            cancel.clone(),
        )
        .await?;
    assert_eq!(completion.values, vec!["42", "420", "4242"]);

    Ok(())
}

#[derive(Default)]
struct TestClientHandler {
    logs: Mutex<Vec<Value>>,
    progress: Mutex<Vec<Value>>,
}

impl McpClientHandler for TestClientHandler {
    fn roots(&self) -> Vec<McpRoot> {
        vec![McpRoot::new(
            "file:///tmp/mcp-ecosystem",
            Some("workspace".to_owned()),
        )]
    }

    fn log_message(&self, params: Value) {
        self.logs.lock().expect("logs lock").push(params);
    }

    fn progress(&self, progress: McpProgressNotification) {
        self.progress.lock().expect("progress lock").push(json!({
            "progressToken": progress.token,
            "progress": progress.progress,
            "total": progress.total,
            "message": progress.message,
        }));
    }
}
