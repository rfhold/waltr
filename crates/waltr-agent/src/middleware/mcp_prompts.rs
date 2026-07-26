//! MCP prompt template middleware.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::{
    Result,
    mcp::McpClientRuntime,
    resource::{
        SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchTarget,
        TemplateSearchResults, mcp_prompt_template, prompt_score,
    },
    template::{TemplateReference, TemplateReferenceRender, template_error},
};

use super::{AgentMiddleware, context::TemplateRenderContext, context::TemplateSearchContext};

#[derive(Debug, Clone)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct McpPromptsMiddleware {
    runtime: Arc<McpClientRuntime>,
}

impl McpPromptsMiddleware {
    pub(crate) const fn new(runtime: Arc<McpClientRuntime>) -> Self {
        Self { runtime }
    }
}

impl AgentMiddleware for McpPromptsMiddleware {
    async fn search_templates(
        &self,
        ctx: &TemplateSearchContext<'_>,
        results: &mut TemplateSearchResults,
    ) -> Result<()> {
        if ctx.request.target != SessionTemplateSearchTarget::Prompts
            || !ctx.request.includes(SessionTemplateKind::Prompt)
        {
            return Ok(());
        }

        let query = ctx
            .request
            .query
            .trim()
            .trim_start_matches('/')
            .to_lowercase();
        for server_prompt in self.runtime.prompts().await {
            let server_name = server_prompt.server_name;
            let prompt = server_prompt.prompt;
            let label = format!("{server_name}:{}", prompt.name);
            let detail = prompt.description_text().to_owned();
            let searchable = format!("{label} {detail}");
            let Some(score) = prompt_score(&label, &searchable, &query) else {
                continue;
            };
            let arguments = prompt
                .arguments
                .iter()
                .map(|argument| (argument.name.clone(), argument.required))
                .collect::<Vec<_>>();
            let Some(template) = mcp_prompt_template(&server_name, &prompt.name, &arguments) else {
                continue;
            };
            let metadata_arguments = prompt
                .arguments
                .iter()
                .map(|argument| {
                    json!({
                        "name": argument.name,
                        "description": argument.description,
                        "required": argument.required,
                    })
                })
                .collect::<Vec<_>>();
            results.push(
                score,
                SessionTemplateOption {
                    kind: SessionTemplateKind::Prompt,
                    label,
                    detail,
                    template,
                    metadata: json!({
                        "promptType": "mcp",
                        "server": server_name,
                        "name": prompt.name,
                        "description": prompt.description,
                        "arguments": metadata_arguments,
                    }),
                },
            );
        }
        Ok(())
    }

    async fn render_template_reference(
        &self,
        ctx: &mut TemplateRenderContext<'_>,
        reference: &TemplateReference,
    ) -> Result<TemplateReferenceRender> {
        let TemplateReference::Prompt {
            namespace,
            name,
            arguments,
            ..
        } = reference
        else {
            return Ok(TemplateReferenceRender::Continue);
        };
        if !self.runtime.has_server(namespace) {
            return Ok(TemplateReferenceRender::Continue);
        }
        if !self.runtime.has_prompt(namespace, name).await {
            return Ok(TemplateReferenceRender::Continue);
        }

        let arguments = match validate_prompt_arguments(arguments.as_ref()) {
            Ok(arguments) => arguments,
            Err(error) => {
                return Ok(TemplateReferenceRender::Rendered(template_error(
                    &format!("mcp prompt {namespace}:{name}"),
                    error,
                )));
            }
        };

        match self
            .runtime
            .get_prompt(namespace, name, arguments, ctx.cancellation_token.clone())
            .await
        {
            Ok(content) => Ok(TemplateReferenceRender::TemplateResult {
                kind: "mcp_prompt".to_owned(),
                tool: None,
                source: Some(format!("{namespace}:{name}")),
                content,
                render_nested: true,
            }),
            Err(error) => Ok(TemplateReferenceRender::Rendered(template_error(
                &format!("mcp prompt {namespace}:{name}"),
                error,
            ))),
        }
    }
}

fn validate_prompt_arguments(
    arguments: Option<&Value>,
) -> std::result::Result<Option<Value>, String> {
    let Some(arguments) = arguments else {
        return Ok(None);
    };
    let Some(arguments) = arguments.as_object() else {
        return Err("arguments must be a JSON object with string values".to_owned());
    };

    let mut validated = Map::new();
    for (name, value) in arguments {
        let Some(value) = value.as_str() else {
            return Err(format!("argument `{name}` must be a string"));
        };
        validated.insert(name.clone(), Value::String(value.to_owned()));
    }

    Ok(Some(Value::Object(validated)))
}
