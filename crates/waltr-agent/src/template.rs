//! Prompt template expansion.

use std::path::Path;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    AgentTool, LlmError, PermissionClaim, PermissionLevel, PermissionOperation, PermissionPattern,
    PermissionPolicy, PermissionResource, PermissionResourcePattern, Result, ToolCall,
    ToolExecutionContext, ToolProgressSink, ToolResult,
    middleware::{
        self, HookOutcome,
        chain::MiddlewareChain,
        context::{MiddlewareStateLog, ToolCallContext, ToolResultContext},
        tools::tool_is_visible,
    },
    permission::{PERMISSION_STATE_NAMESPACE, PermissionRuntimeState},
    tools::filesystem::{FilesystemConfig, ReadInput},
};

const MAX_TEMPLATE_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateSource {
    UserPrompt,
    RuntimePrompt,
}

pub(crate) enum TemplateRender {
    Rendered(String),
    Suspend(middleware::Suspension),
}

enum TemplatePermissionOutcome {
    Allow,
    Deny(String),
    Ask(Vec<PermissionClaim>),
}

/// Runtime values available while expanding prompt templates.
pub(crate) struct TemplateRuntime<'a> {
    /// Base directory used for relative file references.
    pub(crate) base_dir: &'a Path,
    /// Registered host tools callable from templates.
    pub(crate) tools: &'a [AgentTool],
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
                        match self.render_tool_reference(&name, arguments).await? {
                            TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                            TemplateRender::Suspend(suspension) => {
                                return Ok(TemplateRender::Suspend(suspension));
                            }
                        }
                        index += "@tool(".len() + consumed;
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

            if let Some(rest) = remaining.strip_prefix("@\"") {
                match parse_quoted_file_reference(rest) {
                    Ok((path, consumed)) => {
                        match self.render_file_reference(&path, depth + 1).await? {
                            TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                            TemplateRender::Suspend(suspension) => {
                                return Ok(TemplateRender::Suspend(suspension));
                            }
                        }
                        index += "@\"".len() + consumed;
                    }
                    Err(error) => {
                        output.push_str(&template_error("file", error));
                        index += remaining.len();
                    }
                }
                continue;
            }

            if remaining.starts_with('@') && is_reference_boundary(input, index) {
                if let Some((path, consumed)) = parse_bare_file_reference(&remaining[1..]) {
                    match self.render_file_reference(&path, depth + 1).await? {
                        TemplateRender::Rendered(rendered) => output.push_str(&rendered),
                        TemplateRender::Suspend(suspension) => {
                            return Ok(TemplateRender::Suspend(suspension));
                        }
                    }
                    index += 1 + consumed;
                    continue;
                }
            }

            let Some(character) = remaining.chars().next() else {
                break;
            };
            output.push(character);
            index += character.len_utf8();
        }

        Ok(TemplateRender::Rendered(output))
    }

    async fn render_file_reference(&mut self, path: &str, depth: usize) -> Result<TemplateRender> {
        let filesystem = FilesystemConfig::new(self.base_dir);
        match filesystem.read(&ReadInput::new(path)) {
            Ok(content) => {
                match Box::pin(self.render_template_depth(&content, depth)).await? {
                    TemplateRender::Rendered(content) => Ok(TemplateRender::Rendered(
                        template_result("file", Some("read"), Some(path), &content),
                    )),
                    TemplateRender::Suspend(suspension) => Ok(TemplateRender::Suspend(suspension)),
                }
            }
            Err(error) => Ok(TemplateRender::Rendered(template_error(
                &format!("file {path}"),
                error,
            ))),
        }
    }

    async fn render_tool_reference(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<TemplateRender> {
        let Some(tool_index) = self
            .tools
            .iter()
            .position(|tool| tool.definition().name == name)
        else {
            return Ok(TemplateRender::Rendered(template_error(
                "tool",
                format!("unknown tool `{name}`"),
            )));
        };
        let tool_definition = self.tools[tool_index].definition().clone();
        let mut call = ToolCall::new(format!("template-{name}"), name, arguments.clone());

        if !tool_is_visible(self.permission_policy, &tool_definition, true) {
            return Ok(TemplateRender::Rendered(template_error(
                "tool",
                format!("tool is not permitted: {name}"),
            )));
        }

        let mut claims = Vec::new();
        let before_outcome = {
            let mut ctx = ToolCallContext {
                session_id: self.session_id,
                turn: self.turn,
                call: &mut call,
                permission_claims: &mut claims,
                middleware_state: self.middleware_state,
            };
            self.middleware_chain.before_tool_call(&mut ctx).await
        };
        match before_outcome {
            Ok(HookOutcome::Continue) => {}
            Ok(HookOutcome::ReplaceToolResult(result)) => {
                return Ok(TemplateRender::Rendered(template_tool_result(
                    &result.name,
                    &arguments,
                    &result.content,
                )));
            }
            Ok(HookOutcome::Suspend(suspension)) => return Ok(TemplateRender::Suspend(suspension)),
            Err(error) => {
                return Ok(TemplateRender::Rendered(template_error(
                    &format!("tool {name}"),
                    error,
                )));
            }
        }

        match self.permission_outcome(&call, &claims)? {
            TemplatePermissionOutcome::Allow => {}
            TemplatePermissionOutcome::Deny(message) => {
                return Ok(TemplateRender::Rendered(template_error(
                    &format!("tool {name}"),
                    message,
                )));
            }
            TemplatePermissionOutcome::Ask(claims) => {
                if self.source == TemplateSource::RuntimePrompt {
                    return Ok(TemplateRender::Suspend(permission_suspension(
                        self.session_id,
                        self.turn,
                        &call,
                        &claims,
                    )));
                }
            }
        }

        let result = self.tools[tool_index]
            .call(
                ToolExecutionContext::with_progress_and_cancellation(
                    call.clone(),
                    ToolProgressSink::noop(),
                    self.cancellation_token.clone(),
                )
                .with_session_context(self.session_id.into(), self.turn),
            )
            .await;
        let mut result = match result {
            Ok(result) => result,
            Err(error) => error_tool_result(&call, &error),
        };
        let after_outcome = {
            let mut ctx = ToolResultContext {
                call: Some(&call),
                result: &mut result,
                middleware_state: self.middleware_state,
                tools: self.tools,
                middleware_chain: self.middleware_chain.clone(),
                permission_policy: self.permission_policy.clone(),
                cancellation_token: self.cancellation_token.clone(),
                session_id: self.session_id,
                turn: self.turn,
            };
            self.middleware_chain.after_tool_call(&mut ctx).await
        };
        match after_outcome {
            Ok(HookOutcome::Continue) => Ok(TemplateRender::Rendered(template_tool_result(
                name,
                &arguments,
                &result.content,
            ))),
            Ok(HookOutcome::ReplaceToolResult(result)) => Ok(TemplateRender::Rendered(
                template_tool_result(&result.name, &arguments, &result.content),
            )),
            Ok(HookOutcome::Suspend(suspension)) => Ok(TemplateRender::Suspend(suspension)),
            Err(error) => Ok(TemplateRender::Rendered(template_error(
                &format!("tool {name}"),
                error,
            ))),
        }
    }

    fn permission_outcome(
        &mut self,
        tool_call: &ToolCall,
        claims: &[PermissionClaim],
    ) -> Result<TemplatePermissionOutcome> {
        let denied = claims
            .iter()
            .filter(|claim| {
                self.permission_policy
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

        let mut state = self.permission_runtime_state()?;
        let unmatched = claims
            .iter()
            .filter(|claim| !state.patterns.iter().any(|pattern| pattern.matches(claim)))
            .cloned()
            .collect::<Vec<_>>();
        if unmatched.is_empty() {
            return Ok(TemplatePermissionOutcome::Allow);
        }

        if state.consume_one_shot(&tool_call.id) {
            self.set_permission_runtime_state(&state)?;
            return Ok(TemplatePermissionOutcome::Allow);
        }

        let ask = unmatched
            .iter()
            .filter(|claim| {
                self.permission_policy
                    .evaluate_claim_effect(&tool_call.name, claim)
                    .level
                    == PermissionLevel::Ask
            })
            .cloned()
            .collect::<Vec<_>>();
        if ask.is_empty() {
            Ok(TemplatePermissionOutcome::Allow)
        } else {
            Ok(TemplatePermissionOutcome::Ask(ask))
        }
    }

    fn permission_runtime_state(&self) -> Result<PermissionRuntimeState> {
        self.middleware_state
            .get(PERMISSION_STATE_NAMESPACE)
            .map(|state| serde_json::from_value(state.clone()))
            .transpose()
            .map_err(|error| template_runtime_error(error.to_string()))
            .map(Option::unwrap_or_default)
    }

    fn set_permission_runtime_state(&mut self, state: &PermissionRuntimeState) -> Result<()> {
        self.middleware_state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(state)
                .map_err(|error| template_runtime_error(error.to_string()))?,
        );
        Ok(())
    }
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

fn template_error(context: &str, error: impl std::fmt::Display) -> String {
    format!("<template-error>{context}: {error}</template-error>")
}

fn template_result(kind: &str, tool: Option<&str>, source: Option<&str>, content: &str) -> String {
    let mut output = format!("<template-result kind=\"{}\"", escape_xml_attribute(kind));
    if let Some(tool) = tool {
        output.push_str(&format!(" tool=\"{}\"", escape_xml_attribute(tool)));
    }
    if let Some(source) = source {
        output.push_str(&format!(" source=\"{}\"", escape_xml_attribute(source)));
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

fn permission_suspension(
    session_id: &str,
    turn: u64,
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
                    "resource": format!("{:?}", claim.resource),
                    "summary": claim.summary,
                })
            })
            .collect::<Vec<_>>(),
        "patterns": permission_patterns(claims),
    });
    middleware::Suspension::external(
        "permission",
        middleware::ExternalRequestDraft {
            id: format!("permission:{session_id}:{turn}:{}", call.id),
            kind: middleware::ExternalRequestKind::Permission,
            tool_call_id: Some(call.id.clone()),
            prompt: summary,
            payload,
            status: middleware::ExternalRequestStatus::Pending,
        },
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

fn error_tool_result(call: &ToolCall, error: &LlmError) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        call.name.clone(),
        format!("Tool error: {error}\nArguments: {}", call.arguments),
    )
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
