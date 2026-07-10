use std::{net::SocketAddr, sync::Arc};

use axum::Router;
use mcp::client::McpTransportConfig;
use rmcp::{
    ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, CompleteRequestParams, CompleteResult,
        CompletionInfo, ContentBlock, ErrorData, GetPromptRequestParams, GetPromptResult,
        Implementation, JsonObject, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, ListToolsResult, LoggingMessageNotificationParam, NumberOrString,
        PaginatedRequestParams, ProgressNotificationParam, ProgressToken, Prompt, PromptArgument,
        PromptMessage, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        ResourceTemplate, Role, ServerCapabilities, ServerInfo, SetLevelRequestParams, Tool,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    standard_surface::{
        ECHO_TOOL, GREET_PROMPT, ITEM_42, ITEM_TEMPLATE, LOG_TOOL, PROGRESS_TOOL, STATUS_PROMPT,
        STATUS_RESOURCE, item_completions, prompts, resource_templates, resources, tools,
    },
    support::RunningServer,
};

pub struct RmcpStreamableHttpServer {
    address: SocketAddr,
    _task: JoinHandle<()>,
}

impl RmcpStreamableHttpServer {
    pub async fn start() -> mcp::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let service = StreamableHttpService::new(
            || Ok(StandardRmcpServer),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_cancellation_token(CancellationToken::new()),
        );
        let app = Router::new().route_service("/mcp", service);
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::debug!(%error, "rmcp ecosystem test server stopped");
            }
        });
        Ok(Self {
            address,
            _task: task,
        })
    }
}

impl RunningServer for RmcpStreamableHttpServer {
    fn transport(&self) -> McpTransportConfig {
        McpTransportConfig::StreamableHttp {
            endpoint: format!("http://{}/mcp", self.address),
            bearer_token: None,
        }
    }
}

#[derive(Clone)]
struct StandardRmcpServer;

impl ServerHandler for StandardRmcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_completions()
                .enable_logging()
                .build(),
        )
        .with_server_info(Implementation::new("standard-rmcp-server", "0"))
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            tools().into_iter().map(rmcp_tool).collect(),
        ))
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        call_tool(params, context).await
    }

    async fn set_level(
        &self,
        _params: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }

    async fn list_prompts(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(
            prompts().into_iter().map(rmcp_prompt).collect(),
        ))
    }

    async fn get_prompt(
        &self,
        params: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        get_prompt(params)
    }

    async fn list_resources(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(
            resources().into_iter().map(rmcp_resource).collect(),
        ))
    }

    async fn list_resource_templates(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            resource_templates()
                .into_iter()
                .map(rmcp_resource_template)
                .collect(),
        ))
    }

    async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        read_resource(&params.uri)
    }

    async fn complete(
        &self,
        params: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        complete(params)
    }
}

async fn call_tool(
    params: CallToolRequestParams,
    context: RequestContext<RoleServer>,
) -> Result<CallToolResult, ErrorData> {
    let arguments = params.arguments.unwrap_or_default();
    match params.name.as_ref() {
        ECHO_TOOL => Ok(tool_text(string_argument(&arguments, "value")?)),
        LOG_TOOL => {
            let message = string_argument(&arguments, "message")?;
            context
                .peer
                .notify_logging_message(
                    LoggingMessageNotificationParam::new(
                        rmcp::model::LoggingLevel::Warning,
                        json!({ "message": message, "marker": "ecosystem-log-notification-818" }),
                    )
                    .with_logger("ecosystem.test"),
                )
                .await
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            Ok(tool_text(message))
        }
        PROGRESS_TOOL => {
            let token = params
                .meta
                .and_then(|meta| meta.get_progress_token())
                .unwrap_or_else(|| {
                    ProgressToken(NumberOrString::String("ecosystem-progress-token".into()))
                });
            context
                .peer
                .notify_progress(
                    ProgressNotificationParam::new(token, 1.0)
                        .with_total(2.0)
                        .with_message("ecosystem-progress-marker"),
                )
                .await
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            Ok(tool_text(string_argument(&arguments, "value")?))
        }
        _ => Err(ErrorData::invalid_params("unknown test tool", None)),
    }
}

fn get_prompt(params: GetPromptRequestParams) -> Result<GetPromptResult, ErrorData> {
    let text = match params.name.as_ref() {
        STATUS_PROMPT => "ecosystem-prompt-status-313".to_owned(),
        GREET_PROMPT => {
            let arguments = params.arguments.unwrap_or_default();
            let name = string_argument(&arguments, "name")?;
            format!("hello {name} from ecosystem-prompt-414")
        }
        _ => return Err(ErrorData::invalid_params("unknown test prompt", None)),
    };
    Ok(GetPromptResult::new(vec![PromptMessage::new_text(
        Role::User,
        text,
    )]))
}

fn read_resource(uri: &str) -> Result<ReadResourceResult, ErrorData> {
    let text = match uri {
        STATUS_RESOURCE => "ecosystem-resource-status-616",
        ITEM_42 => "ecosystem-resource-template-item-42-717",
        _ => return Err(ErrorData::invalid_params("unknown test resource", None)),
    };
    Ok(ReadResourceResult::new(vec![
        ResourceContents::text(text, uri).with_mime_type("text/plain"),
    ]))
}

fn complete(params: CompleteRequestParams) -> Result<CompleteResult, ErrorData> {
    let values =
        if params.r#ref.as_resource_uri() == Some(ITEM_TEMPLATE) && params.argument.name == "id" {
            item_completions(&params.argument.value)
        } else {
            Vec::new()
        };
    let mut completion =
        CompletionInfo::new(values).map_err(|error| ErrorData::internal_error(error, None))?;
    completion.total = Some(completion.values.len() as u32);
    completion.has_more = Some(false);
    Ok(CompleteResult::new(completion))
}

fn rmcp_tool(tool: mcp::McpToolDefinition) -> Tool {
    let input_schema = tool.input_schema.as_object().cloned().unwrap_or_default();
    Tool::new(
        tool.name,
        tool.description.unwrap_or_default(),
        input_schema,
    )
}

fn rmcp_prompt(prompt: mcp::McpPromptDefinition) -> Prompt {
    Prompt::new(
        prompt.name,
        prompt.description,
        Some(
            prompt
                .arguments
                .into_iter()
                .map(|argument| {
                    let mut prompt_argument = PromptArgument::new(argument.name);
                    if let Some(description) = argument.description {
                        prompt_argument = prompt_argument.with_description(description);
                    }
                    if argument.required {
                        prompt_argument = prompt_argument.with_required(true);
                    }
                    prompt_argument
                })
                .collect(),
        ),
    )
}

fn rmcp_resource(resource: mcp::McpResourceDefinition) -> Resource {
    let mut rmcp_resource = Resource::new(resource.uri, resource.name);
    if let Some(title) = resource.title {
        rmcp_resource = rmcp_resource.with_title(title);
    }
    if let Some(description) = resource.description {
        rmcp_resource = rmcp_resource.with_description(description);
    }
    if let Some(mime_type) = resource.mime_type {
        rmcp_resource = rmcp_resource.with_mime_type(mime_type);
    }
    if let Some(size) = resource.size {
        rmcp_resource = rmcp_resource.with_size(size);
    }
    rmcp_resource
}

fn rmcp_resource_template(
    resource_template: mcp::McpResourceTemplateDefinition,
) -> ResourceTemplate {
    let mut rmcp_resource_template =
        ResourceTemplate::new(resource_template.uri_template, resource_template.name);
    if let Some(title) = resource_template.title {
        rmcp_resource_template = rmcp_resource_template.with_title(title);
    }
    if let Some(description) = resource_template.description {
        rmcp_resource_template = rmcp_resource_template.with_description(description);
    }
    if let Some(mime_type) = resource_template.mime_type {
        rmcp_resource_template = rmcp_resource_template.with_mime_type(mime_type);
    }
    rmcp_resource_template
}

fn tool_text(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn string_argument(arguments: &JsonObject, field: &str) -> Result<String, ErrorData> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            ErrorData::invalid_params(format!("missing string argument `{field}`"), None)
        })
}
