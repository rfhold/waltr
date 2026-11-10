//! Prompt template expansion.

use std::{fmt::Write as _, path::Path};

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

use crate::agent_host::{
    Capability, PermissionPolicy, Result, ToolCall, ToolResult,
    middleware::{
        HookOutcome,
        chain::MiddlewareChain,
        context::{MiddlewareStateLog, TemplateRenderContext, ToolResultContext},
    },
};

const MAX_TEMPLATE_DEPTH: usize = 8;
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
}

/// Values shared across templates rendered during one run.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct TemplateRunContext<'a> {
    pub(crate) middleware_chain: &'a MiddlewareChain,
    pub(crate) permission_policy: &'a PermissionPolicy,
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) session_id: &'a str,
    pub(crate) turn: u64,
    pub(crate) sandbox: Option<&'a crate::agent_host::sandbox_gateway::SandboxHandle>,
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
        let effect_name = match &reference {
            TemplateReference::Tool { name, .. } => name.as_str(),
            TemplateReference::File { .. } => "file",
            TemplateReference::Resource { .. } => "resource",
            TemplateReference::Prompt { .. } => "prompt",
        };
        let ordinal = self.occurrence;
        self.occurrence += 1;
        let id = format!("template-{effect_name}-{}-{ordinal}", self.occurrence_scope);
        self.rendered_tool_ids.push(id.clone());
        let call_id = Some(id);
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
        }
    }
}

#[allow(clippy::redundant_pub_crate, clippy::too_many_lines)]
pub(crate) async fn render_tool_template_reference(
    ctx: &mut TemplateRenderContext<'_>,
    _tools: &[Capability],
    name: &str,
    arguments: Value,
) -> Result<TemplateReferenceRender> {
    let result = execute_template_capability(ctx, name, arguments.clone()).await?;
    Ok(TemplateReferenceRender::Rendered(
        render_template_tool_result(&result, &arguments),
    ))
}

pub(crate) async fn execute_template_capability(
    ctx: &mut TemplateRenderContext<'_>,
    name: &str,
    arguments: Value,
) -> Result<ToolResult> {
    let dispatcher = ctx.middleware_chain.dispatcher()?.clone();
    let Some(tool) = dispatcher
        .agent_tools()
        .into_iter()
        .find(|tool| tool.definition().name == name)
    else {
        return Ok(ToolResult::new(
            ctx.tool_call_id.unwrap_or("template-tool"),
            name,
            format!("unknown tool `{name}`"),
        )
        .with_error());
    };
    let call = ToolCall::new(
        ctx.tool_call_id.unwrap_or("template-tool"),
        name,
        arguments.clone(),
    );
    let mut result = ctx
        .middleware_state
        .execute_effect(tool, call.clone(), ctx.turn)
        .await?;
    let chain = ctx.middleware_chain;
    let outcome = chain
        .after_tool_call(&mut ToolResultContext {
            call: Some(&call),
            result: &mut result,
            middleware_state: ctx.middleware_state,
            middleware_chain: chain.clone(),
            permission_policy: ctx.permission_policy.clone(),
            cancellation_token: ctx.cancellation_token.clone(),
            session_id: ctx.session_id,
            turn: ctx.turn,
        })
        .await?;
    match outcome {
        HookOutcome::Continue => Ok(result),
        HookOutcome::ReplaceToolResult(result) => Ok(result),
    }
}

fn render_template_tool_result(result: &ToolResult, arguments: &Value) -> String {
    if result.is_error {
        template_tool_error_result(&result.name, arguments, &result.content)
    } else {
        template_tool_result(&result.name, arguments, &result.content)
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
    use std::{future::Future, path::Path};

    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;

    use crate::agent_host::{
        ToolResult,
        middleware::{AgentMiddleware, HookOutcome, context::ToolCallContext},
        sandbox_gateway::SandboxGateway,
        tools::{filesystem::FilesystemToolKind, sandbox},
    };

    use super::{TemplateOrigin, render_template_tool_result};

    struct RedirectWrite;

    impl AgentMiddleware for RedirectWrite {
        fn before_tool_call(
            &self,
            ctx: &mut ToolCallContext<'_>,
        ) -> impl Future<Output = crate::agent_host::Result<HookOutcome>> + Send {
            ctx.call.arguments["filePath"] = json!("redirected");
            async { Ok(HookOutcome::Continue) }
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
        let provider = std::sync::Arc::new(
            crate::host_tests::scripted_provider::ScriptedProvider::new("template", |request| {
                assert!(request.prompt.contains("transformed"));
                vec![Ok(waltr_agent::CompletionEvent::Finished)]
            }),
        );
        let host = crate::host_tests::harness::HostSession::new(
            crate::agent_host::HostRuntimeBuilder::new(
                waltr_agent::ModelRef::new(
                    waltr_agent::ProviderId::OpenAi,
                    "test-model",
                    waltr_agent::ModelApi::OpenAiResponses,
                ),
                provider,
            )
            .with_file_templates()
            .with_sandbox(handle.clone())
            .with_tool(sandbox::agent_tool(
                FilesystemToolKind::Write.definition(),
                handle,
            ))
            .with_middleware(RedirectWrite),
        )
        .await;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            host.start("@tool(write, {\"filePath\":\"original\",\"content\":\"transformed\"})"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.into_iter().all(|event| event.is_ok()));
        assert!(!root.path().join("original").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("redirected")).unwrap(),
            "transformed"
        );
        host.close().await;
        gateway.shutdown();
        worker.abort();
        server.abort();
        let _joined = tokio::join!(worker, server);
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
