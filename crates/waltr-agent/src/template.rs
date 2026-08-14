//! Prompt template expansion.

use std::{fmt::Write as _, path::Path};

use serde_json::{Value, json};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) enum TemplateSource {
    UserPrompt,
    RuntimePrompt,
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

/// Runtime values available while expanding prompt templates.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct TemplateRuntime<'a> {
    /// Base directory used for relative file references.
    pub(crate) base_dir: &'a Path,
    pub(crate) middleware_chain: &'a MiddlewareChain,
    pub(crate) permission_policy: &'a PermissionPolicy,
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
    /// Cancellation token shared with the agent run.
    pub(crate) cancellation_token: CancellationToken,
    /// Durable session id to attach to templated tool calls.
    pub(crate) session_id: &'a str,
    /// Current provider turn to attach to templated tool calls.
    pub(crate) turn: u64,
    pub(crate) source: TemplateSource,
}

impl TemplateRuntime<'_> {
    /// Expand file and tool references in one prompt template.
    pub(crate) async fn render_template(&mut self, input: &str) -> Result<TemplateRender> {
        self.render_template_depth(input, 0).await
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

            if self.source == TemplateSource::UserPrompt
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
        let middleware_chain = self.middleware_chain;
        let permission_policy = self.permission_policy;
        let cancellation_token = self.cancellation_token.clone();
        let mut ctx = TemplateRenderContext {
            base_dir: self.base_dir,
            source: self.source,
            middleware_state: self.middleware_state,
            middleware_chain,
            permission_policy,
            cancellation_token,
            session_id: self.session_id,
            turn: self.turn,
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
    let mut call = ToolCall::new(format!("template-{name}"), name, arguments.clone());

    if !tool_is_visible(ctx.permission_policy, &tool_definition, true) {
        return Ok(TemplateReferenceRender::Rendered(
            template_tool_error_result(name, &arguments, format!("tool is not permitted: {name}")),
        ));
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
            } else if ctx.source == TemplateSource::RuntimePrompt {
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

    match permission_outcome(ctx, &call, &claims)? {
        TemplatePermissionOutcome::Allow => {}
        TemplatePermissionOutcome::Deny(message) => {
            return Ok(TemplateReferenceRender::Rendered(
                template_tool_error_result(name, &arguments, message),
            ));
        }
        TemplatePermissionOutcome::Ask(claims) => {
            if ctx.source == TemplateSource::RuntimePrompt {
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

    let result = tools[tool_index]
        .call(
            ToolExecutionContext::with_progress_and_cancellation(
                call.clone(),
                ToolProgressSink::noop(),
                ctx.cancellation_token.clone(),
            )
            .with_session_context(ctx.session_id.into(), ctx.turn),
        )
        .await;
    let mut result = match result {
        Ok(result) => result,
        Err(error) => error_tool_result(&call, &error),
    };
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
    match after_outcome {
        Ok(HookOutcome::Continue) => Ok(TemplateReferenceRender::Rendered(
            render_template_tool_result(&result, &arguments),
        )),
        Ok(HookOutcome::ReplaceToolResult(result)) => Ok(TemplateReferenceRender::Rendered(
            render_template_tool_result(&result, &arguments),
        )),
        Ok(HookOutcome::Suspend(suspension)) => Ok(TemplateReferenceRender::Suspend(suspension)),
        Err(error) => Ok(TemplateReferenceRender::Rendered(
            template_tool_error_result(name, &arguments, error),
        )),
    }
}

fn render_template_tool_result(result: &ToolResult, arguments: &Value) -> String {
    if result.is_error {
        template_tool_error_result(&result.name, arguments, &result.content)
    } else {
        template_tool_result(&result.name, arguments, &result.content)
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
    let identity = serde_json::to_string(&(request.stage, request.policy.identity(), claims))
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
    use serde_json::json;

    use crate::ToolResult;

    use super::render_template_tool_result;

    #[test]
    fn structured_tool_error_uses_the_error_template() {
        let result = ToolResult::new("call-1", "camera", "bad <snapshot>").with_error();

        let rendered = render_template_tool_result(&result, &json!({ "entity": "camera.front" }));

        assert!(rendered.contains("status=\"error\""));
        assert!(rendered.contains("bad &lt;snapshot&gt;"));
        assert!(!rendered.contains("bad <snapshot>"));
    }
}
