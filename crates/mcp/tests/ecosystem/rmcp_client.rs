use std::sync::{Arc, Mutex};

use mcp::client::McpTransportConfig;
use rmcp::{
    ClientHandler,
    model::{
        CallToolRequestParams, ClientInfo, ContentBlock, JsonObject, ListRootsResult, LoggingLevel,
        LoggingMessageNotificationParam, Meta, NumberOrString, ProgressNotificationParam,
        ProgressToken, ReadResourceRequestParams, Root, SetLevelRequestParams,
    },
    serve_client,
    service::{NotificationContext, RequestContext, RoleClient},
    transport::StreamableHttpClientTransport,
};
use serde_json::{Value, json};

use super::standard_surface::{
    ECHO_TOOL, GREET_PROMPT, ITEM_42, ITEM_TEMPLATE, LOG_TOOL, PROGRESS_TOOL, STATUS_RESOURCE,
};

pub async fn assert_standard_surface(transport: McpTransportConfig) -> mcp::Result<()> {
    let McpTransportConfig::StreamableHttp { endpoint, .. } = transport else {
        return Err(mcp::Error::protocol(
            "rmcp ecosystem test requires streamable HTTP",
        ));
    };

    let handler = RmcpTestClientHandler::default();
    let logs = Arc::clone(&handler.logs);
    let progress = Arc::clone(&handler.progress);
    let transport = StreamableHttpClientTransport::from_uri(endpoint);
    let peer = serve_client(handler, transport)
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;

    let tools = peer
        .list_all_tools()
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert!(tools.iter().any(|tool| tool.name == ECHO_TOOL));
    assert!(tools.iter().any(|tool| tool.name == LOG_TOOL));
    assert!(tools.iter().any(|tool| tool.name == PROGRESS_TOOL));

    let echo = peer
        .call_tool(call_tool_params(
            ECHO_TOOL,
            json!({ "value": "ecosystem-tool-marker-707" }),
        ))
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert_text_contains(&echo.content, "ecosystem-tool-marker-707")?;

    let log = peer
        .call_tool(call_tool_params(
            LOG_TOOL,
            json!({ "message": "ecosystem-log-marker-818" }),
        ))
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert_text_contains(&log.content, "ecosystem-log-marker-818")?;

    let mut progress_call = call_tool_params(
        PROGRESS_TOOL,
        json!({ "value": "ecosystem-progress-result-919" }),
    );
    progress_call.meta = Some(Meta::with_progress_token(ProgressToken(
        NumberOrString::String("ecosystem-progress-token".into()),
    )));
    let progress_result = peer
        .call_tool(progress_call)
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert_text_contains(&progress_result.content, "ecosystem-progress-result-919")?;

    let prompts = peer
        .list_all_prompts()
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert!(prompts.iter().any(|prompt| prompt.name == "status_prompt"));
    assert!(prompts.iter().any(|prompt| prompt.name == GREET_PROMPT));

    let prompt = peer
        .get_prompt(
            rmcp::model::GetPromptRequestParams::new(GREET_PROMPT)
                .with_arguments(json_object(json!({ "name": "ecosystem" }))?),
        )
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert_prompt_contains(
        &prompt.messages,
        "hello ecosystem from ecosystem-prompt-414",
    )?;

    let resources = peer
        .list_all_resources()
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert!(
        resources
            .iter()
            .any(|resource| resource.uri == STATUS_RESOURCE)
    );

    let templates = peer
        .list_all_resource_templates()
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert!(
        templates
            .iter()
            .any(|template| template.uri_template == ITEM_TEMPLATE)
    );

    let resource = peer
        .read_resource(ReadResourceRequestParams::new(STATUS_RESOURCE))
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert_resource_contains(&resource.contents, "ecosystem-resource-status-616")?;

    let templated = peer
        .read_resource(ReadResourceRequestParams::new(ITEM_42))
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert_resource_contains(
        &templated.contents,
        "ecosystem-resource-template-item-42-717",
    )?;

    let completion = peer
        .complete_resource_argument(ITEM_TEMPLATE, "id", "42", None)
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;
    assert_eq!(completion.values, vec!["42", "420", "4242"]);

    peer.set_level(SetLevelRequestParams::new(LoggingLevel::Warning))
        .await
        .map_err(|error| mcp::Error::protocol(error.to_string()))?;

    assert!(
        logs.lock()
            .expect("logs lock")
            .iter()
            .any(|value| value.to_string().contains("ecosystem-log-notification-818"))
    );
    assert!(
        progress
            .lock()
            .expect("progress lock")
            .iter()
            .any(|value| value.to_string().contains("ecosystem-progress-marker"))
    );

    Ok(())
}

#[derive(Default)]
struct RmcpTestClientHandler {
    logs: Arc<Mutex<Vec<Value>>>,
    progress: Arc<Mutex<Vec<Value>>>,
}

impl ClientHandler for RmcpTestClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }

    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, rmcp::model::ErrorData> {
        Ok(ListRootsResult::new(vec![
            Root::new("file:///tmp/mcp-ecosystem").with_name("workspace"),
        ]))
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.logs.lock().expect("logs lock").push(json!(params));
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.progress
            .lock()
            .expect("progress lock")
            .push(json!(params));
    }
}

fn call_tool_params(name: &'static str, arguments: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name)
        .with_arguments(json_object(arguments).expect("object arguments"))
}

fn json_object(value: Value) -> mcp::Result<JsonObject> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| mcp::Error::protocol(format!("expected object arguments: {value:#}")))
}

fn assert_text_contains(content: &[ContentBlock], needle: &str) -> mcp::Result<()> {
    if content.iter().any(|content| {
        content
            .as_text()
            .is_some_and(|text| text.text.contains(needle))
    }) {
        return Ok(());
    }
    Err(mcp::Error::protocol(format!(
        "missing text {needle}: {content:#?}"
    )))
}

fn assert_prompt_contains(
    messages: &[rmcp::model::PromptMessage],
    needle: &str,
) -> mcp::Result<()> {
    if messages.iter().any(|message| {
        message
            .content
            .as_text()
            .is_some_and(|text| text.text.contains(needle))
    }) {
        return Ok(());
    }
    Err(mcp::Error::protocol(format!(
        "missing prompt text {needle}: {messages:#?}"
    )))
}

fn assert_resource_contains(
    contents: &[rmcp::model::ResourceContents],
    needle: &str,
) -> mcp::Result<()> {
    if contents.iter().any(|content| match content {
        rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.contains(needle),
        rmcp::model::ResourceContents::BlobResourceContents { .. } => false,
        _ => false,
    }) {
        return Ok(());
    }
    Err(mcp::Error::protocol(format!(
        "missing resource text {needle}: {contents:#?}"
    )))
}
