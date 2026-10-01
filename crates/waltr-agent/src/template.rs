//! Prompt template expansion.

use std::{collections::BTreeMap, fmt::Write as _, path::Path};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    AgentTool, LlmError, PermissionClaim, PermissionLevel, PermissionOperation, PermissionPattern,
    PermissionPolicy, PermissionResource, PermissionResourcePattern, Result, ToolCall,
    ToolExecutionContext, ToolProgressSink, ToolResult,
    middleware::{
        self, HookOutcome,
        chain::MiddlewareChain,
        context::{MiddlewareStateLog, TemplateRenderContext, ToolCallContext, ToolResultContext},
        tools::tool_is_visible,
    },
    permission::{
        PERMISSION_STATE_NAMESPACE, PermissionApprovalStage, PermissionClaimIdentity,
        PermissionRuntimeState, permission_claim_identities,
    },
};

const MAX_TEMPLATE_DEPTH: usize = 8;
const TEMPLATE_TOOL_RESULTS: &str = "template_tool_results";
const TEMPLATE_RESOURCE_READER_RESULTS: &str = "template_resource_reader_results";

#[derive(serde::Serialize, serde::Deserialize)]
struct TemplateResourceReaderResult {
    binding: Value,
    rendered: String,
}
const TEMPLATE_SANDBOX_RESULTS: &str = "template_sandbox_results";

#[allow(clippy::redundant_pub_crate)]
pub(crate) enum TemplateOrigin<'a> {
    UserPrompt {
        submission: u64,
        prompt_index: usize,
    },
    Instruction {
        path: &'a Path,
    },
}

impl TemplateOrigin<'_> {
    fn scope(&self, turn: u64, input: &str) -> String {
        let mut scope = match self {
            Self::UserPrompt {
                submission,
                prompt_index,
            } => {
                format!("user-{submission}-{prompt_index}")
            }
            Self::Instruction { path } => format!("instruction-{turn}-{}", path.display()),
        };
        for byte in Sha256::digest(input.as_bytes()) {
            write!(scope, "{byte:02x}").expect("writing to a string cannot fail");
        }
        scope
    }
}

#[allow(clippy::redundant_pub_crate)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum TemplateRender {
    Rendered(String),
    Suspend(middleware::Suspension),
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) enum TemplateReference {
    File {
        path: String,
        raw: String,
    },
    Tool {
        name: String,
        arguments: Value,
        raw: String,
    },
    Resource {
        server: String,
        uri: String,
        raw: String,
    },
    Prompt {
        namespace: String,
        name: String,
        arguments: Option<Value>,
        raw: String,
    },
}

impl TemplateReference {
    pub(crate) fn raw(&self) -> &str {
        match self {
            Self::File { raw, .. }
            | Self::Tool { raw, .. }
            | Self::Resource { raw, .. }
            | Self::Prompt { raw, .. } => raw,
        }
    }
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) enum TemplateReferenceRender {
    Continue,
    Rendered(String),
    TemplateResult {
        kind: String,
        tool: Option<String>,
        source: Option<String>,
        content: String,
        render_nested: bool,
    },
    Suspend(middleware::Suspension),
}

enum TemplatePermissionOutcome {
    Allow,
    Deny(String),
    Ask(Vec<PermissionClaim>),
}

/// Values shared across templates rendered during one run.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct TemplateRunContext<'a> {
    pub(crate) middleware_chain: &'a MiddlewareChain,
    pub(crate) permission_policy: &'a PermissionPolicy,
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) session_id: &'a str,
    pub(crate) turn: u64,
    pub(crate) sandbox: Option<&'a crate::sandbox_gateway::SandboxHandle>,
}

/// Runtime values available while expanding one prompt template.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct TemplateRuntime<'a> {
    pub(crate) base_dir: &'a Path,
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
    pub(crate) run: TemplateRunContext<'a>,
    pub(crate) origin: TemplateOrigin<'a>,
    occurrence_scope: String,
    occurrence: u64,
    rendered_tool_ids: Vec<String>,
}

impl TemplateRuntime<'_> {
    pub(crate) fn new<'a>(
        base_dir: &'a Path,
        middleware_state: &'a mut MiddlewareStateLog,
        run: TemplateRunContext<'a>,
        origin: TemplateOrigin<'a>,
    ) -> TemplateRuntime<'a> {
        TemplateRuntime {
            base_dir,
            middleware_state,
            run,
            origin,
            occurrence_scope: String::new(),
            occurrence: 0,
            rendered_tool_ids: Vec::new(),
        }
    }

    /// Expand file and tool references in one prompt template.
    pub(crate) async fn render_template(&mut self, input: &str) -> Result<TemplateRender> {
        self.occurrence_scope = self.origin.scope(self.run.turn, input);
        self.occurrence = 0;
        self.rendered_tool_ids.clear();
        self.render_template_depth(input, 0).await
    }

    pub(crate) fn rendered_tool_ids(&self) -> &[String] {
        &self.rendered_tool_ids
    }

    #[allow(clippy::too_many_lines)]
    async fn render_template_depth(&mut self, input: &str, depth: usize) -> Result<TemplateRender> {
        if depth >= MAX_TEMPLATE_DEPTH {
            return Ok(TemplateRender::Rendered(input.to_owned()));
        }

        let mut output = String::new();
        let mut index = 0;
        while index < input.len() {
            let remaining = &input[index..];
            if let Some(rest) = remaining.strip_prefix("@tool(") {
                match parse_tool_reference(rest) {
                    Ok((name, arguments, consumed)) => {
                        let consumed = "@tool(".len() + consumed;
                        let reference = TemplateReference::Tool {
                            name,
                            arguments,
                            raw: remaining[..consumed].to_owned(),
                        };
                        match self.render_reference(reference, depth).await? {
                            TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                            TemplateRender::Suspend(suspension) => {
                                return Ok(TemplateRender::Suspend(suspension));
                            }
                        }
                        index += consumed;
                    }
                    Err(error) => {
                        output.push_str(&template_error("tool", error));
                        index += remaining
                            .find(')')
                            .map_or(remaining.len(), |position| position + 1);
                    }
                }
                continue;
            }

            if let Some(rest) = remaining.strip_prefix("@resource(") {
                match parse_resource_reference(rest) {
                    Ok((server, uri, consumed)) => {
                        let consumed = "@resource(".len() + consumed;
                        let reference = TemplateReference::Resource {
                            server,
                            uri,
                            raw: remaining[..consumed].to_owned(),
                        };
                        match self.render_reference(reference, depth).await? {
                            TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                            TemplateRender::Suspend(suspension) => {
                                return Ok(TemplateRender::Suspend(suspension));
                            }
                        }
                        index += consumed;
                    }
                    Err(error) => {
                        output.push_str(&template_error("resource", error));
                        index += remaining
                            .find(')')
                            .map_or(remaining.len(), |position| position + 1);
                    }
                }
                continue;
            }

            if matches!(self.origin, TemplateOrigin::UserPrompt { .. })
                && is_first_line(input, index)
                && remaining.starts_with('/')
                && is_reference_boundary(input, index)
                && let Some((namespace, name, arguments, consumed)) =
                    parse_prompt_reference(remaining)
            {
                let reference = TemplateReference::Prompt {
                    namespace,
                    name,
                    arguments,
                    raw: remaining[..consumed].to_owned(),
                };
                match self.render_reference(reference, depth).await? {
                    TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                    TemplateRender::Suspend(suspension) => {
                        return Ok(TemplateRender::Suspend(suspension));
                    }
                }
                index += consumed;
                continue;
            }

            if let Some(rest) = remaining.strip_prefix("@\"") {
                match parse_quoted_file_reference(rest) {
                    Ok((path, consumed)) => {
                        let consumed = "@\"".len() + consumed;
                        let reference = TemplateReference::File {
                            path,
                            raw: remaining[..consumed].to_owned(),
                        };
                        match self.render_reference(reference, depth).await? {
                            TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                            TemplateRender::Suspend(suspension) => {
                                return Ok(TemplateRender::Suspend(suspension));
                            }
                        }
                        index += consumed;
                    }
                    Err(error) => {
                        output.push_str(&template_error("file", error));
                        index += remaining.len();
                    }
                }
                continue;
            }

            if remaining.starts_with('@')
                && is_reference_boundary(input, index)
                && let Some((path, consumed)) = parse_bare_file_reference(&remaining[1..])
            {
                let consumed = 1 + consumed;
                let reference = TemplateReference::File {
                    path,
                    raw: remaining[..consumed].to_owned(),
                };
                match self.render_reference(reference, depth).await? {
                    TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                    TemplateRender::Suspend(suspension) => {
                        return Ok(TemplateRender::Suspend(suspension));
                    }
                }
                index += consumed;
                continue;
            }

            let Some(character) = remaining.chars().next() else {
                break;
            };
            output.push(character);
            index += character.len_utf8();
        }

        Ok(TemplateRender::Rendered(output))
    }

    async fn render_reference(
        &mut self,
        reference: TemplateReference,
        depth: usize,
    ) -> Result<TemplateRender> {
        let call_id = match &reference {
            TemplateReference::Tool { name, .. } => {
                let ordinal = self.occurrence;
                self.occurrence += 1;
                let id = format!("template-{name}-{}-{ordinal}", self.occurrence_scope);
                self.rendered_tool_ids.push(id.clone());
                Some(id)
            }
            _ => None,
        };
        let middleware_chain = self.run.middleware_chain;
        let permission_policy = self.run.permission_policy;
        let cancellation_token = self.run.cancellation_token.clone();
        let mut ctx = TemplateRenderContext {
            base_dir: self.base_dir,
            middleware_state: self.middleware_state,
            middleware_chain,
            permission_policy,
            cancellation_token,
            session_id: self.run.session_id,
            turn: self.run.turn,
            sandbox: self.run.sandbox,
            tool_call_id: call_id.as_deref(),
        };
        match middleware_chain
            .render_template_reference(&mut ctx, &reference)
            .await?
        {
            TemplateReferenceRender::Continue => Ok(TemplateRender::Rendered(match &reference {
                TemplateReference::Prompt { .. } => reference.raw().to_owned(),
                TemplateReference::Resource { uri, .. } => template_error(
                    &format!("resource {uri}"),
                    "no middleware rendered the reference",
                ),
                TemplateReference::File { path, .. } => template_error(
                    &format!("file {path}"),
                    "no middleware rendered the reference",
                ),
                TemplateReference::Tool {
                    name, arguments, ..
                } => template_tool_error_result(name, arguments, format!("unknown tool `{name}`")),
            })),
            TemplateReferenceRender::Rendered(rendered) => Ok(TemplateRender::Rendered(rendered)),
            TemplateReferenceRender::TemplateResult {
                kind,
                tool,
                source,
                content,
                render_nested,
            } => {
                let content = if render_nested {
                    match Box::pin(self.render_template_depth(&content, depth + 1)).await? {
                        TemplateRender::Rendered(content) => content,
                        TemplateRender::Suspend(suspension) => {
                            return Ok(TemplateRender::Suspend(suspension));
                        }
                    }
                } else {
                    content
                };
                Ok(TemplateRender::Rendered(template_result(
                    &kind,
                    tool.as_deref(),
                    source.as_deref(),
                    &content,
                )))
            }
            TemplateReferenceRender::Suspend(suspension) => Ok(TemplateRender::Suspend(suspension)),
        }
    }
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn clear_sandbox_template_results(
    state: &mut MiddlewareStateLog,
    tool_ids: &[String],
) -> Result<()> {
    let Some(value) = state.get(TEMPLATE_SANDBOX_RESULTS) else {
        return Ok(());
    };
    let mut completed: BTreeMap<String, String> =
        serde_json::from_value(value.clone()).map_err(template_runtime_error)?;
    let mut removed = false;
    for id in tool_ids {
        removed |= completed.remove(id).is_some();
    }
    if removed {
        state.set(
            TEMPLATE_SANDBOX_RESULTS,
            serde_json::to_value(completed).map_err(template_runtime_error)?,
        );
    }
    Ok(())
}

#[allow(clippy::redundant_pub_crate, clippy::too_many_lines)]
pub(crate) async fn render_tool_template_reference(
    ctx: &mut TemplateRenderContext<'_>,
    tools: &[AgentTool],
    name: &str,
    arguments: Value,
) -> Result<TemplateReferenceRender> {
    let Some(tool_index) = tools.iter().position(|tool| tool.definition().name == name) else {
        return Ok(TemplateReferenceRender::Rendered(
            template_tool_error_result(name, &arguments, format!("unknown tool `{name}`")),
        ));
    };
    let tool_definition = tools[tool_index].definition().clone();
    let mut call = ToolCall::new(
        ctx.tool_call_id.unwrap_or("template-tool"),
        name,
        arguments.clone(),
    );

    if !tool_is_visible(ctx.permission_policy, &tool_definition, true) {
        return Ok(TemplateReferenceRender::Rendered(
            template_tool_error_result(name, &arguments, format!("tool is not permitted: {name}")),
        ));
    }

    // A later reference may Ask and restart rendering from the beginning.
    let cache_namespace = if tools[tool_index].is_sandbox_backed() {
        TEMPLATE_SANDBOX_RESULTS
    } else {
        TEMPLATE_TOOL_RESULTS
    };
    let completed: BTreeMap<String, String> = ctx
        .middleware_state
        .get(cache_namespace)
        .map(|value| serde_json::from_value(value.clone()).map_err(template_runtime_error))
        .transpose()?
        .unwrap_or_default();
    if name == "read_mcp_resource" {
        let readers: BTreeMap<String, TemplateResourceReaderResult> = ctx
            .middleware_state
            .get(TEMPLATE_RESOURCE_READER_RESULTS)
            .map(|value| serde_json::from_value(value.clone()).map_err(template_runtime_error))
            .transpose()?
            .unwrap_or_default();
        if let Some(cached) = readers.get(&call.id)
            && cached.binding == template_reader_binding(ctx, &call)
            && !ctx
                .permission_policy
                .evaluate_input(name, &arguments)
                .is_some_and(|input| input.effect.level == PermissionLevel::Deny)
        {
            return Ok(TemplateReferenceRender::Rendered(cached.rendered.clone()));
        }
    } else if let Some(rendered) = completed.get(&call.id) {
        return Ok(TemplateReferenceRender::Rendered(rendered.clone()));
    }

    let whole_tool_level = ctx.permission_policy.evaluate_tool(&call.name);
    let whole_tool_request = PermissionRequestContext {
        session_id: ctx.session_id,
        turn: ctx.turn,
        stage: PermissionApprovalStage::WholeTool,
        policy: ctx.permission_policy,
    };
    let whole_tool_request_id = template_permission_request_id(&whole_tool_request, &call, &[]);
    let mut whole_tool_approved = false;
    match whole_tool_level {
        PermissionLevel::Allow => {}
        PermissionLevel::Deny => {
            return Ok(TemplateReferenceRender::Rendered(
                template_tool_error_result(
                    name,
                    &arguments,
                    format!("tool is not permitted: {name}"),
                ),
            ));
        }
        PermissionLevel::Ask => {
            let state = permission_runtime_state(ctx.middleware_state)?;
            if state.has_one_shot(
                &whole_tool_request_id,
                &call.id,
                PermissionApprovalStage::WholeTool,
                &[],
            ) {
                whole_tool_approved = true;
            } else {
                return Ok(TemplateReferenceRender::Suspend(permission_suspension(
                    &whole_tool_request,
                    &call,
                    &[],
                )));
            }
        }
    }

    let mut claims = Vec::new();
    let before_outcome = {
        let middleware_chain = ctx.middleware_chain;
        let mut tool_ctx = ToolCallContext {
            approved_tool_name: name,
            permission_policy: ctx.permission_policy,
            contextual_input_is_approved,
            contextual_suspension,
            cancellation_token: ctx.cancellation_token.clone(),
            session_id: ctx.session_id,
            turn: ctx.turn,
            call: &mut call,
            permission_claims: &mut claims,
            middleware_state: ctx.middleware_state,
        };
        middleware_chain.before_tool_call(&mut tool_ctx).await
    };
    match before_outcome {
        Ok(HookOutcome::Continue) => {}
        Ok(HookOutcome::ReplaceToolResult(result)) => {
            if name == "read_mcp_resource" {
                match permission_outcome(ctx, &call, &claims)? {
                    TemplatePermissionOutcome::Allow => {}
                    TemplatePermissionOutcome::Deny(message) => {
                        return Ok(TemplateReferenceRender::Rendered(
                            template_tool_error_result(name, &arguments, message),
                        ));
                    }
                    TemplatePermissionOutcome::Ask(claims) => {
                        return Ok(TemplateReferenceRender::Suspend(permission_suspension(
                            &PermissionRequestContext {
                                session_id: ctx.session_id,
                                turn: ctx.turn,
                                stage: PermissionApprovalStage::Contextual,
                                policy: ctx.permission_policy,
                            },
                            &call,
                            &claims,
                        )));
                    }
                }
                if whole_tool_approved {
                    let mut state = permission_runtime_state(ctx.middleware_state)?;
                    if state.consume_one_shot(
                        &whole_tool_request_id,
                        &call.id,
                        PermissionApprovalStage::WholeTool,
                        &[],
                    ) {
                        set_permission_runtime_state(ctx.middleware_state, &state)?;
                    }
                }
                if !result.is_error {
                    cache_template_reader_result(
                        ctx,
                        &call,
                        render_template_tool_result(&result, &arguments),
                    )?;
                }
            }
            return Ok(TemplateReferenceRender::Rendered(
                render_template_tool_result(&result, &arguments),
            ));
        }
        Ok(HookOutcome::Suspend(suspension)) => {
            return Ok(TemplateReferenceRender::Suspend(suspension));
        }
        Err(error) => {
            return Ok(TemplateReferenceRender::Rendered(
                template_tool_error_result(name, &arguments, error),
            ));
        }
    }
    if call.name != name {
        return Ok(TemplateReferenceRender::Rendered(
            template_tool_error_result(
                name,
                &arguments,
                "middleware changed the approved tool name",
            ),
        ));
    }

    let prepared = if tools[tool_index].is_sandbox_backed() {
        let Some(sandbox) = ctx.sandbox else {
            return Ok(TemplateReferenceRender::Rendered(
                template_tool_error_result(name, &arguments, "workspace tool requires sandbox"),
            ));
        };
        match sandbox
            .prepare_with_shell_defaults(
                &call.name,
                &call.arguments,
                tools[tool_index].shell_defaults(),
            )
            .await
        {
            Ok(prepared) => {
                match crate::tools::sandbox::claims(&prepared) {
                    Ok(extra) => claims.extend(extra),
                    Err(error) => {
                        prepared.cancel(sandbox);
                        return Ok(TemplateReferenceRender::Rendered(
                            template_tool_error_result(name, &arguments, error),
                        ));
                    }
                }
                Some(prepared)
            }
            Err(error) => {
                return Ok(TemplateReferenceRender::Rendered(
                    template_tool_error_result(name, &arguments, error),
                ));
            }
        }
    } else {
        None
    };

    match permission_outcome(ctx, &call, &claims)? {
        TemplatePermissionOutcome::Allow => {}
        TemplatePermissionOutcome::Deny(message) => {
            return Ok(TemplateReferenceRender::Rendered(
                template_tool_error_result(name, &arguments, message),
            ));
        }
        TemplatePermissionOutcome::Ask(claims) => {
            return Ok(TemplateReferenceRender::Suspend(permission_suspension(
                &PermissionRequestContext {
                    session_id: ctx.session_id,
                    turn: ctx.turn,
                    stage: PermissionApprovalStage::Contextual,
                    policy: ctx.permission_policy,
                },
                &call,
                &claims,
            )));
        }
    }

    if whole_tool_approved {
        let mut state = permission_runtime_state(ctx.middleware_state)?;
        if state.consume_one_shot(
            &whole_tool_request_id,
            &call.id,
            PermissionApprovalStage::WholeTool,
            &[],
        ) {
            set_permission_runtime_state(ctx.middleware_state, &state)?;
        }
    }

    let mut tool_context = ToolExecutionContext::with_progress_and_cancellation(
        call.clone(),
        ToolProgressSink::noop(),
        ctx.cancellation_token.clone(),
    )
    .with_session_context(ctx.session_id.into(), ctx.turn);
    if let Some(prepared) = prepared {
        tool_context = tool_context.with_prepared_sandbox_call(prepared);
    }
    let result = tools[tool_index].call(tool_context).await;
    let mut result = match result {
        Ok(result) => result,
        Err(error) => error_tool_result(&call, &error),
    };
    // Shell timeouts are reported in the worker output rather than as tool errors.
    if tools[tool_index].is_sandbox_backed()
        && call.name == "shell"
        && result.content.starts_with("Timed out: true\n")
    {
        result.is_error = true;
    }
    let succeeded = !result.is_error;
    let after_outcome = {
        let middleware_chain = ctx.middleware_chain;
        let mut result_ctx = ToolResultContext {
            call: Some(&call),
            result: &mut result,
            middleware_state: ctx.middleware_state,
            middleware_chain: middleware_chain.clone(),
            permission_policy: ctx.permission_policy.clone(),
            cancellation_token: ctx.cancellation_token.clone(),
            session_id: ctx.session_id,
            turn: ctx.turn,
        };
        middleware_chain.after_tool_call(&mut result_ctx).await
    };
    let (rendered, cacheable) = match after_outcome {
        Ok(HookOutcome::Continue) => (render_template_tool_result(&result, &arguments), succeeded),
        Ok(HookOutcome::ReplaceToolResult(result)) => {
            let cacheable = succeeded && !result.is_error;
            (render_template_tool_result(&result, &arguments), cacheable)
        }
        Ok(HookOutcome::Suspend(suspension)) => {
            // Execute completed; retrying this render must not dispatch the ticket again.
            if tools[tool_index].is_sandbox_backed() && succeeded {
                let mut completed = completed;
                completed.insert(call.id, render_template_tool_result(&result, &arguments));
                ctx.middleware_state.set(
                    cache_namespace,
                    serde_json::to_value(completed).map_err(template_runtime_error)?,
                );
            }
            return Ok(TemplateReferenceRender::Suspend(suspension));
        }
        Err(error) => (template_tool_error_result(name, &arguments, error), false),
    };
    if name == "read_mcp_resource" && cacheable {
        cache_template_reader_result(ctx, &call, rendered.clone())?;
    } else if name != "read_mcp_resource" && (!tools[tool_index].is_sandbox_backed() || cacheable) {
        let mut completed = completed;
        completed.insert(call.id, rendered.clone());
        ctx.middleware_state.set(
            cache_namespace,
            serde_json::to_value(completed).map_err(template_runtime_error)?,
        );
    }
    Ok(TemplateReferenceRender::Rendered(rendered))
}

fn render_template_tool_result(result: &ToolResult, arguments: &Value) -> String {
    if result.is_error {
        template_tool_error_result(&result.name, arguments, &result.content)
    } else {
        template_tool_result(&result.name, arguments, &result.content)
    }
}

fn template_reader_binding(ctx: &TemplateRenderContext<'_>, call: &ToolCall) -> Value {
    json!([
        ctx.session_id,
        ctx.turn,
        call.id,
        call.name,
        call.arguments,
        ctx.permission_policy.identity()
    ])
}

fn cache_template_reader_result(
    ctx: &mut TemplateRenderContext<'_>,
    call: &ToolCall,
    rendered: String,
) -> Result<()> {
    let mut readers: BTreeMap<String, TemplateResourceReaderResult> = ctx
        .middleware_state
        .get(TEMPLATE_RESOURCE_READER_RESULTS)
        .map(|value| serde_json::from_value(value.clone()).map_err(template_runtime_error))
        .transpose()?
        .unwrap_or_default();
    readers.insert(
        call.id.clone(),
        TemplateResourceReaderResult {
            binding: template_reader_binding(ctx, call),
            rendered,
        },
    );
    ctx.middleware_state.set(
        TEMPLATE_RESOURCE_READER_RESULTS,
        serde_json::to_value(readers).map_err(template_runtime_error)?,
    );
    Ok(())
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn template_read_permission_error(
    ctx: &TemplateRenderContext<'_>,
    name: &str,
    arguments: &Value,
) -> Result<Option<String>> {
    match ctx.permission_policy.evaluate_tool(name) {
        PermissionLevel::Allow => {}
        PermissionLevel::Deny => return Ok(Some(format!("tool is not permitted: {name}"))),
        PermissionLevel::Ask => {
            return Ok(Some(format!(
                "permission required: invoke {name} directly for approval"
            )));
        }
    }
    let Some(input) = ctx.permission_policy.evaluate_input(name, arguments) else {
        return Ok(None);
    };
    match input.effect.level {
        PermissionLevel::Allow => Ok(None),
        PermissionLevel::Deny => Ok(Some(
            "permission denied: tool input does not satisfy policy".to_owned(),
        )),
        PermissionLevel::Ask => {
            let state = permission_runtime_state(ctx.middleware_state)?;
            if input
                .claims
                .iter()
                .all(|claim| state.patterns.iter().any(|pattern| pattern.matches(claim)))
            {
                return Ok(None);
            }
            // Resource references have no tool-call identity for one-shot approval.
            Ok(Some(format!(
                "permission required: invoke {name} directly for approval"
            )))
        }
    }
}

fn permission_outcome(
    ctx: &mut TemplateRenderContext<'_>,
    tool_call: &ToolCall,
    claims: &[PermissionClaim],
) -> Result<TemplatePermissionOutcome> {
    let denied = claims
        .iter()
        .filter(|claim| {
            ctx.permission_policy
                .evaluate_claim_effect(&tool_call.name, claim)
                .level
                == PermissionLevel::Deny
        })
        .cloned()
        .collect::<Vec<_>>();
    if !denied.is_empty() {
        return Ok(TemplatePermissionOutcome::Deny(permission_claim_message(
            "permission denied",
            &denied,
        )));
    }

    let input = ctx
        .permission_policy
        .evaluate_input(&tool_call.name, &tool_call.arguments);
    if input
        .as_ref()
        .is_some_and(|evaluation| evaluation.effect.level == PermissionLevel::Deny)
    {
        return Ok(TemplatePermissionOutcome::Deny(
            "permission denied: tool input does not satisfy policy".to_owned(),
        ));
    }

    let mut state = permission_runtime_state(ctx.middleware_state)?;
    let input_claims = input
        .as_ref()
        .filter(|input| input.effect.level == PermissionLevel::Ask)
        .map_or(&[][..], |input| input.claims.as_slice());
    let unmatched = claims
        .iter()
        .filter(|claim| {
            ctx.permission_policy
                .evaluate_claim_refinement_effect(&tool_call.name, claim)
                .is_some_and(|effect| effect.level == PermissionLevel::Ask)
        })
        .cloned()
        .chain(input_claims.iter().cloned())
        .filter(|claim| !state.patterns.iter().any(|pattern| pattern.matches(claim)))
        .collect::<Vec<_>>();
    let identities = permission_claim_identities(&unmatched);
    let request_id = template_permission_request_id(
        &PermissionRequestContext {
            session_id: ctx.session_id,
            turn: ctx.turn,
            stage: PermissionApprovalStage::Contextual,
            policy: ctx.permission_policy,
        },
        tool_call,
        &identities,
    );
    if state.consume_one_shot(
        &request_id,
        &tool_call.id,
        PermissionApprovalStage::Contextual,
        &identities,
    ) {
        set_permission_runtime_state(ctx.middleware_state, &state)?;
        return Ok(TemplatePermissionOutcome::Allow);
    }
    if unmatched.is_empty() {
        return Ok(TemplatePermissionOutcome::Allow);
    }

    Ok(TemplatePermissionOutcome::Ask(unmatched))
}

fn permission_runtime_state(
    middleware_state: &MiddlewareStateLog,
) -> Result<PermissionRuntimeState> {
    middleware_state
        .get(PERMISSION_STATE_NAMESPACE)
        .map(|state| serde_json::from_value(state.clone()))
        .transpose()
        .map_err(|error| template_runtime_error(error.to_string()))
        .map(Option::unwrap_or_default)
}

fn set_permission_runtime_state(
    middleware_state: &mut MiddlewareStateLog,
    state: &PermissionRuntimeState,
) -> Result<()> {
    middleware_state.set(
        PERMISSION_STATE_NAMESPACE,
        serde_json::to_value(state).map_err(|error| template_runtime_error(error.to_string()))?,
    );
    Ok(())
}

fn parse_tool_reference(input: &str) -> std::result::Result<(String, Value, usize), String> {
    let Some(comma) = input.find(',') else {
        return Err("missing comma after tool name".to_owned());
    };
    let name = input[..comma].trim();
    if name.is_empty() {
        return Err("missing tool name".to_owned());
    }
    let mut json_start = comma + 1;
    while input[json_start..].starts_with(char::is_whitespace) {
        json_start += input[json_start..].chars().next().map_or(0, char::len_utf8);
    }
    let (json, json_len) = parse_json_value(&input[json_start..])?;
    let mut consumed = json_start + json_len;
    while input[consumed..].starts_with(char::is_whitespace) {
        consumed += input[consumed..].chars().next().map_or(0, char::len_utf8);
    }
    if !input[consumed..].starts_with(')') {
        return Err("missing closing `)` after tool arguments".to_owned());
    }
    consumed += 1;
    Ok((name.to_owned(), json, consumed))
}

fn parse_resource_reference(input: &str) -> std::result::Result<(String, String, usize), String> {
    let Some(comma) = input.find(',') else {
        return Err("missing comma after resource server".to_owned());
    };
    let server = input[..comma].trim();
    if !is_template_name(server) {
        return Err("invalid resource server".to_owned());
    }
    let mut uri_start = comma + 1;
    while input[uri_start..].starts_with(char::is_whitespace) {
        uri_start += input[uri_start..].chars().next().map_or(0, char::len_utf8);
    }
    let (uri, uri_len) = parse_json_value(&input[uri_start..])?;
    let Some(uri) = uri.as_str() else {
        return Err("resource URI must be a JSON string".to_owned());
    };
    let mut consumed = uri_start + uri_len;
    while input[consumed..].starts_with(char::is_whitespace) {
        consumed += input[consumed..].chars().next().map_or(0, char::len_utf8);
    }
    if !input[consumed..].starts_with(')') {
        return Err("missing closing `)` after resource URI".to_owned());
    }
    consumed += 1;
    Ok((server.to_owned(), uri.to_owned(), consumed))
}

fn parse_json_value(input: &str) -> std::result::Result<(Value, usize), String> {
    let deserializer = serde_json::Deserializer::from_str(input);
    let mut stream = deserializer.into_iter::<Value>();
    match stream.next() {
        Some(Ok(value)) => Ok((value, stream.byte_offset())),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("missing JSON tool arguments".to_owned()),
    }
}

fn parse_quoted_file_reference(input: &str) -> std::result::Result<(String, usize), String> {
    let mut path = String::new();
    let mut escaped = false;
    for (offset, character) in input.char_indices() {
        if escaped {
            path.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Ok((path, offset + 1)),
            _ => path.push(character),
        }
    }
    Err("unterminated quoted file reference".to_owned())
}

fn is_reference_boundary(input: &str, index: usize) -> bool {
    index == 0
        || input[..index]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

fn is_first_line(input: &str, index: usize) -> bool {
    !input[..index].contains('\n')
}

fn parse_prompt_name(input: &str) -> Option<(String, usize)> {
    let mut consumed = 0;
    for character in input.chars() {
        if is_template_name_character(character) {
            consumed += character.len_utf8();
        } else {
            break;
        }
    }
    (consumed > 0).then(|| (input[..consumed].to_owned(), consumed))
}

fn is_template_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_template_name_character)
}

const fn is_template_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn parse_prompt_reference(input: &str) -> Option<(String, String, Option<Value>, usize)> {
    let rest = input.strip_prefix('/')?;
    let (namespace, namespace_len) = parse_prompt_name(rest)?;
    let name_start = namespace_len + 1;
    if !rest[namespace_len..].starts_with(':') {
        return None;
    }
    let (name, name_len) = parse_prompt_name(&rest[name_start..])?;
    let mut consumed = 1 + name_start + name_len;
    if !input[consumed..].starts_with('(') {
        return Some((namespace, name, None, consumed));
    }
    let (arguments, arguments_len) = parse_json_value(&input[consumed + 1..]).ok()?;
    consumed += 1 + arguments_len;
    while input[consumed..].starts_with(char::is_whitespace) {
        consumed += input[consumed..].chars().next().map_or(0, char::len_utf8);
    }
    if !input[consumed..].starts_with(')') {
        return None;
    }
    consumed += 1;
    Some((namespace, name, Some(arguments), consumed))
}

fn parse_bare_file_reference(input: &str) -> Option<(String, usize)> {
    let mut consumed = 0;
    for character in input.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '/' | '_' | '-') {
            consumed += character.len_utf8();
        } else {
            break;
        }
    }
    let path = &input[..consumed];
    if consumed == 0 || !is_bare_file_reference(path) {
        None
    } else {
        Some((path.to_owned(), consumed))
    }
}

fn is_bare_file_reference(path: &str) -> bool {
    (path.contains('/') || path.contains('.'))
        && path.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '/' | '_' | '-')
        })
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn template_error(context: &str, error: impl std::fmt::Display) -> String {
    format!("<template-error>{context}: {error}</template-error>")
}

fn template_result(kind: &str, tool: Option<&str>, source: Option<&str>, content: &str) -> String {
    let mut output = format!("<template-result kind=\"{}\"", escape_xml_attribute(kind));
    if let Some(tool) = tool {
        write!(output, " tool=\"{}\"", escape_xml_attribute(tool))
            .expect("writing to a string cannot fail");
    }
    if let Some(source) = source {
        write!(output, " source=\"{}\"", escape_xml_attribute(source))
            .expect("writing to a string cannot fail");
    }
    output.push_str(">\n");
    output.push_str(content);
    output.push_str("\n</template-result>");
    output
}

fn template_tool_result(name: &str, arguments: &Value, content: &str) -> String {
    let arguments = serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string());
    format!(
        "<template-result kind=\"tool\" tool=\"{}\">\n<arguments>{}</arguments>\n<content>\n{}\n</content>\n</template-result>",
        escape_xml_attribute(name),
        escape_xml_text(&arguments),
        content,
    )
}

fn template_tool_error_result(
    name: &str,
    arguments: &Value,
    error: impl std::fmt::Display,
) -> String {
    let arguments = serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string());
    format!(
        "<template-result kind=\"tool\" tool=\"{}\" status=\"error\">\n<arguments>{}</arguments>\n<content>\n{}\n</content>\n</template-result>",
        escape_xml_attribute(name),
        escape_xml_text(&arguments),
        escape_xml_text(&error.to_string()),
    )
}

struct PermissionRequestContext<'a> {
    session_id: &'a str,
    turn: u64,
    stage: PermissionApprovalStage,
    policy: &'a PermissionPolicy,
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn contextual_input_is_approved(
    ctx: &ToolCallContext<'_>,
    claims: &[PermissionClaim],
    state: &PermissionRuntimeState,
) -> bool {
    let identities = permission_claim_identities(claims);
    let request_id = template_permission_request_id(
        &PermissionRequestContext {
            session_id: ctx.session_id,
            turn: ctx.turn,
            stage: PermissionApprovalStage::Contextual,
            policy: ctx.permission_policy,
        },
        ctx.call,
        &identities,
    );
    state.has_one_shot(
        &request_id,
        &ctx.call.id,
        PermissionApprovalStage::Contextual,
        &identities,
    )
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn contextual_suspension(
    ctx: &ToolCallContext<'_>,
    claims: &[PermissionClaim],
) -> middleware::Suspension {
    permission_suspension(
        &PermissionRequestContext {
            session_id: ctx.session_id,
            turn: ctx.turn,
            stage: PermissionApprovalStage::Contextual,
            policy: ctx.permission_policy,
        },
        ctx.call,
        claims,
    )
}

fn permission_suspension(
    request: &PermissionRequestContext<'_>,
    call: &ToolCall,
    claims: &[PermissionClaim],
) -> middleware::Suspension {
    let summary = permission_claim_message("permission required", claims);
    let payload = json!({
        "template": true,
        "tool_call": {
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        },
        "claims": claims
            .iter()
            .map(|claim| {
                json!({
                    "operation": format!("{:?}", claim.operation),
                    "resource": permission_claim_resource(&claim.resource),
                    "summary": claim.summary,
                })
            })
            .collect::<Vec<_>>(),
        "patterns": permission_patterns(claims),
        "claim_identities": permission_claim_identities(claims),
    });
    middleware::Suspension::external(
        "permission",
        middleware::ExternalRequestDraft {
            id: template_permission_request_id(request, call, &permission_claim_identities(claims)),
            kind: middleware::ExternalRequestKind::Permission,
            tool_call_id: Some(call.id.clone()),
            prompt: summary,
            payload,
            status: middleware::ExternalRequestStatus::Pending,
        },
    )
}

fn template_permission_request_id(
    request: &PermissionRequestContext<'_>,
    call: &ToolCall,
    claims: &[PermissionClaimIdentity],
) -> String {
    let identity = serde_json::to_string(&(
        request.stage,
        request.policy.identity(),
        &call.name,
        &call.arguments,
        claims,
    ))
    .expect("permission request identity serializes");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in identity.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!(
        "permission:{}:{}:{}:{hash:016x}",
        request.session_id, request.turn, call.id
    )
}

fn permission_patterns(claims: &[PermissionClaim]) -> Vec<PermissionPattern> {
    let mut patterns = Vec::new();
    for claim in claims {
        let pattern = permission_pattern(claim);
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    patterns
}

fn permission_pattern(claim: &PermissionClaim) -> PermissionPattern {
    let resource = match &claim.resource {
        PermissionResource::FilesystemPath(path) => {
            let parent = path.parent().unwrap_or(path.as_path()).to_path_buf();
            PermissionResourcePattern::FilesystemPathPrefix(parent)
        }
        PermissionResource::FilesystemPathPrefix(path) => {
            PermissionResourcePattern::FilesystemPathPrefix(path.clone())
        }
        PermissionResource::WorkingDirectory(path) => {
            PermissionResourcePattern::WorkingDirectoryPrefix(path.clone())
        }
        PermissionResource::ShellCommand(command) => {
            return PermissionPattern {
                operation: Some(PermissionOperation::Execute),
                resource: PermissionResourcePattern::ShellCommand(command.clone()),
            };
        }
        PermissionResource::ToolInput {
            tool,
            pointer,
            value,
        } => PermissionResourcePattern::ToolInput {
            tool: tool.clone(),
            pointer: pointer.clone(),
            value: value.clone(),
        },
    };
    PermissionPattern {
        operation: Some(claim.operation),
        resource,
    }
}

fn permission_claim_message(prefix: &str, claims: &[PermissionClaim]) -> String {
    let summaries = claims
        .iter()
        .map(|claim| claim.summary.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    if summaries.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {summaries}")
    }
}

fn permission_claim_resource(resource: &PermissionResource) -> String {
    match resource {
        PermissionResource::ToolInput { tool, pointer, .. } => {
            format!("ToolInput {{ tool: {tool:?}, pointer: {pointer:?} }}")
        }
        resource => format!("{resource:?}"),
    }
}

fn error_tool_result(call: &ToolCall, error: &LlmError) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        call.name.clone(),
        format!("Tool error: {error}\nArguments: {}", call.arguments),
    )
    .with_error()
}

fn template_runtime_error(error: impl std::fmt::Display) -> LlmError {
    LlmError::Protocol {
        provider: "template",
        message: error.to_string(),
    }
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use futures_util::{StreamExt, stream};
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tokio_util::sync::CancellationToken;

    use crate::{
        ToolResult,
        middleware::{
            AgentMiddleware, HookOutcome,
            chain::MiddlewareChain,
            context::{
                MiddlewareStateLog, TemplateRenderContext, ToolCallContext, ToolResultContext,
            },
        },
        sandbox_gateway::SandboxGateway,
        tools::{filesystem::FilesystemToolKind, sandbox},
    };

    use super::{
        TemplateOrigin, TemplateReferenceRender, render_template_tool_result,
        render_tool_template_reference,
    };

    #[test]
    fn template_permission_identity_binds_tool_name_and_complete_arguments() {
        let policy = crate::PermissionPolicy::default();
        let request = super::PermissionRequestContext {
            session_id: "session",
            turn: 0,
            stage: super::PermissionApprovalStage::Contextual,
            policy: &policy,
        };
        let original = crate::ToolCall::new(
            "same-occurrence",
            "read_mcp_resource",
            json!({"server":"waltr","uri":"waltr://resources"}),
        );
        let approved = super::template_permission_request_id(&request, &original, &[]);
        assert_eq!(
            approved,
            super::template_permission_request_id(&request, &original, &[])
        );
        for changed in [
            crate::ToolCall::new(
                "same-occurrence",
                "other_reader",
                original.arguments.clone(),
            ),
            crate::ToolCall::new(
                "same-occurrence",
                "read_mcp_resource",
                json!({"server":"waltr","uri":"waltr://sessions"}),
            ),
            crate::ToolCall::new(
                "same-occurrence",
                "read_mcp_resource",
                json!({"server":"other","uri":"waltr://resources"}),
            ),
        ] {
            assert_ne!(
                approved,
                super::template_permission_request_id(&request, &changed, &[])
            );
        }
    }

    struct RedirectWrite;

    impl AgentMiddleware for RedirectWrite {
        fn before_tool_call(
            &self,
            ctx: &mut ToolCallContext<'_>,
        ) -> impl Future<Output = crate::Result<HookOutcome>> + Send {
            ctx.call.arguments["filePath"] = json!("redirected");
            async { Ok(HookOutcome::Continue) }
        }
    }

    struct SuspendAfterWrite(Arc<AtomicUsize>);

    struct TemplateProvider(Arc<AtomicUsize>);

    impl crate::LlmStream for TemplateProvider {
        fn name(&self) -> &'static str {
            "template-post-hook"
        }

        fn stream_request<'a>(
            &'a self,
            request: crate::CompletionRequest<'a>,
        ) -> crate::BoxStreamFuture<'a> {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert!(request.prompt.contains("after hook"), "{request:?}");
            Box::pin(async {
                Ok(
                    Box::pin(stream::iter(vec![Ok(crate::CompletionEvent::Finished)]))
                        as crate::BoxCompletionStream,
                )
            })
        }
    }

    impl AgentMiddleware for SuspendAfterWrite {
        fn after_tool_call(
            &self,
            ctx: &mut ToolResultContext<'_>,
        ) -> impl Future<Output = crate::Result<HookOutcome>> + Send {
            self.0.fetch_add(1, Ordering::SeqCst);
            ctx.result.content.push_str(" after hook");
            async {
                Ok(HookOutcome::Suspend(
                    crate::middleware::Suspension::external(
                        "approval",
                        crate::middleware::ExternalRequestDraft {
                            id: "post-hook-approval".into(),
                            kind: crate::middleware::ExternalRequestKind::Custom(
                                "post-hook".into(),
                            ),
                            tool_call_id: None,
                            prompt: "Approve result".into(),
                            payload: json!({}),
                            status: crate::middleware::ExternalRequestStatus::Pending,
                        },
                    ),
                ))
            }
        }
    }

    #[tokio::test]
    async fn sandbox_template_prepares_middleware_transformed_arguments() {
        let root = tempfile::tempdir().unwrap();
        let gateway = SandboxGateway::new("test".into());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = gateway.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service.service())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let path = root.path().to_path_buf();
        let worker = tokio::spawn(async move {
            waltr_agent_sandbox::run_worker(channel, path, "test")
                .await
                .unwrap();
        });
        gateway.wait_ready().await.unwrap();

        let handle = gateway.handle();
        let tools = [sandbox::agent_tool(
            FilesystemToolKind::Write.definition(),
            handle.clone(),
        )];
        let mut chain = MiddlewareChain::new();
        chain.push(RedirectWrite);
        let policy = crate::PermissionPolicy::default();
        let mut state = MiddlewareStateLog::default();
        let mut ctx = TemplateRenderContext {
            base_dir: root.path(),
            middleware_state: &mut state,
            middleware_chain: &chain,
            permission_policy: &policy,
            cancellation_token: CancellationToken::new(),
            session_id: "test",
            turn: 0,
            sandbox: Some(&handle),
            tool_call_id: Some("template-write"),
        };
        let output = render_tool_template_reference(
            &mut ctx,
            &tools,
            "write",
            json!({"filePath":"original","content":"transformed"}),
        )
        .await
        .unwrap();
        assert!(
            matches!(output, TemplateReferenceRender::Rendered(result) if !result.contains("status=\"error\""))
        );
        assert!(!root.path().join("original").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("redirected")).unwrap(),
            "transformed"
        );
        gateway.shutdown();
        worker.abort();
        server.abort();
    }

    #[tokio::test]
    async fn sandbox_template_after_tool_suspension_reuses_completed_result() {
        let root = tempfile::tempdir().unwrap();
        let gateway = SandboxGateway::new("test".into());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = gateway.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service.service())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let path = root.path().to_path_buf();
        let worker = tokio::spawn(async move {
            waltr_agent_sandbox::run_worker(channel, path, "test")
                .await
                .unwrap();
        });
        gateway.wait_ready().await.unwrap();

        let handle = gateway.handle();
        let tools = [sandbox::agent_tool(
            FilesystemToolKind::Write.definition(),
            handle.clone(),
        )];
        let calls = Arc::new(AtomicUsize::new(0));
        let mut chain = MiddlewareChain::new();
        chain.push(SuspendAfterWrite(calls.clone()));
        let policy = crate::PermissionPolicy::default();
        let mut state = MiddlewareStateLog::default();
        let mut ctx = TemplateRenderContext {
            base_dir: root.path(),
            middleware_state: &mut state,
            middleware_chain: &chain,
            permission_policy: &policy,
            cancellation_token: CancellationToken::new(),
            session_id: "test",
            turn: 0,
            sandbox: Some(&handle),
            tool_call_id: Some("template-suspended-write"),
        };
        let arguments = json!({"filePath":"marker", "content":"executed once"});
        let first = render_tool_template_reference(&mut ctx, &tools, "write", arguments.clone())
            .await
            .unwrap();
        assert!(
            matches!(first, TemplateReferenceRender::Suspend(suspension) if suspension.external_request.is_some())
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "executed once"
        );
        std::fs::write(root.path().join("marker"), "outside change").unwrap();

        let resumed = render_tool_template_reference(&mut ctx, &tools, "write", arguments)
            .await
            .unwrap();
        assert!(
            matches!(resumed, TemplateReferenceRender::Rendered(result) if result.contains("after hook"))
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "outside change"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        gateway.shutdown();
        worker.abort();
        server.abort();
    }

    #[tokio::test]
    async fn sandbox_template_after_tool_suspension_resumes_without_reexecution() {
        let root = tempfile::tempdir().unwrap();
        let gateway = SandboxGateway::new("test".into());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = gateway.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service.service())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let path = root.path().to_path_buf();
        let worker = tokio::spawn(async move {
            waltr_agent_sandbox::run_worker(channel, path, "test")
                .await
                .unwrap();
        });
        gateway.wait_ready().await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let agent = crate::AgentRuntime::builder(
            crate::ModelRef::new(
                crate::ProviderId::OpenAi,
                "test-model",
                crate::ModelApi::OpenAiResponses,
            ),
            Arc::new(TemplateProvider(provider_calls.clone())),
        )
        .with_sandbox(gateway.handle())
        .with_filesystem_tools(crate::tools::filesystem::FilesystemConfig::new(root.path()))
        .with_middleware(SuspendAfterWrite(calls.clone()))
        .build();
        let store = crate::MemorySessionStore::new();
        let sessions = crate::SessionCoordinator::new(store.clone());
        let id = sessions
            .create(crate::SessionCreateRequest::new(
                crate::SessionId::generate(),
            ))
            .await
            .unwrap()
            .id()
            .clone();
        let events = sessions
            .prompt(
                &agent,
                id.clone(),
                "@tool(write, {\"filePath\":\"marker\",\"content\":\"executed once\"})",
            )
            .stream()
            .collect::<Vec<_>>()
            .await;
        let request_id = events
            .iter()
            .map(|event| event.as_ref().unwrap())
            .find_map(|event| match &event.content {
                crate::TimelineContent::ExternalRequest { request } => Some(request.id.clone()),
                _ => None,
            })
            .expect("post-tool suspension request");
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "executed once"
        );
        std::fs::write(root.path().join("marker"), "outside change").unwrap();
        let resumed = sessions
            .resume(
                &agent,
                id,
                crate::ResumeInput::ExternalResponse {
                    request_id,
                    response: crate::ExternalResponse::new(json!("approved")),
                },
            )
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(resumed.iter().all(Result::is_ok), "{resumed:?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "outside change"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
        gateway.shutdown();
        worker.abort();
        server.abort();
    }

    #[test]
    fn template_occurrence_scope_is_stable_and_separates_sources() {
        let input = "@tool(camera, {})";
        let user = TemplateOrigin::UserPrompt {
            submission: 10,
            prompt_index: 2,
        };
        let instruction = TemplateOrigin::Instruction {
            path: Path::new("/workspace/AGENTS.md"),
        };
        assert_eq!(user.scope(4, input), user.scope(4, input));
        assert_ne!(
            user.scope(4, input),
            user.scope(4, "@tool(camera, {\"x\":1})")
        );
        assert_ne!(user.scope(4, input), instruction.scope(4, input));
        assert_ne!(
            instruction.scope(4, input),
            TemplateOrigin::Instruction {
                path: Path::new("/workspace/nested/AGENTS.md")
            }
            .scope(4, input)
        );
    }

    #[test]
    fn structured_tool_error_uses_the_error_template() {
        let result = ToolResult::new("call-1", "camera", "bad <snapshot>").with_error();

        let rendered = render_template_tool_result(&result, &json!({ "entity": "camera.front" }));

        assert!(rendered.contains("status=\"error\""));
        assert!(rendered.contains("bad &lt;snapshot&gt;"));
        assert!(!rendered.contains("bad <snapshot>"));
    }
}
