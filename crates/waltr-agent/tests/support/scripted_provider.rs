use std::sync::Mutex;

use futures_util::stream;
use waltr_agent::{
    BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest, LlmStream,
    ToolDefinition,
};
use serde_json::Value;

type RequestHandler = Box<
    dyn for<'a> Fn(&CompletionRequest<'a>) -> Vec<waltr_agent::Result<CompletionEvent>>
        + Send
        + Sync,
>;
type RequestProjector = Box<dyn for<'a> Fn(&CompletionRequest<'a>) -> Option<Value> + Send + Sync>;

pub struct ScriptedProvider {
    name: &'static str,
    handler: RequestHandler,
    projector: Option<RequestProjector>,
}

impl ScriptedProvider {
    pub fn new(
        name: &'static str,
        handler: impl for<'a> Fn(&CompletionRequest<'a>) -> Vec<waltr_agent::Result<CompletionEvent>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            name,
            handler: Box::new(handler),
            projector: None,
        }
    }

    pub fn from_turns(name: &'static str, script: Vec<Vec<CompletionEvent>>) -> Self {
        let requests = Mutex::new(0);
        Self::new(name, move |_| {
            let mut request_count = requests.lock().expect("request lock");
            let index = *request_count;
            *request_count += 1;
            drop(request_count);
            let events = script
                .get(index)
                .unwrap_or_else(|| panic!("unexpected provider request {index}"))
                .clone();
            events.into_iter().map(Ok).collect()
        })
    }

    #[allow(dead_code)]
    pub fn with_projection(
        mut self,
        projector: impl for<'a> Fn(&CompletionRequest<'a>) -> Option<Value> + Send + Sync + 'static,
    ) -> Self {
        self.projector = Some(Box::new(projector));
        self
    }
}

impl LlmStream for ScriptedProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            Ok(Box::pin(stream::iter((self.handler)(&request))) as BoxCompletionStream)
        })
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        self.projector
            .as_ref()
            .and_then(|projector| projector(request))
    }
}

pub struct CapturingProvider<P> {
    inner: P,
    tools_by_turn: Mutex<Vec<Vec<ToolDefinition>>>,
    request_prompts: Mutex<Vec<String>>,
    system_prompts: Mutex<Vec<Option<String>>>,
}

impl<P> CapturingProvider<P> {
    pub const fn new(inner: P) -> Self {
        Self {
            inner,
            tools_by_turn: Mutex::new(Vec::new()),
            request_prompts: Mutex::new(Vec::new()),
            system_prompts: Mutex::new(Vec::new()),
        }
    }

    #[allow(dead_code)]
    pub fn request_prompts(&self) -> Vec<String> {
        self.request_prompts
            .lock()
            .expect("captured request prompts")
            .clone()
    }

    pub fn system_prompts(&self) -> Vec<Option<String>> {
        self.system_prompts
            .lock()
            .expect("captured system prompts")
            .clone()
    }

    pub fn all_tools(&self) -> Vec<ToolDefinition> {
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .iter()
            .flatten()
            .cloned()
            .collect()
    }

    pub fn all_tool_names(&self) -> Vec<String> {
        self.all_tools().into_iter().map(|tool| tool.name).collect()
    }

    #[allow(dead_code)]
    pub fn tool_named(&self, name: &str) -> Option<ToolDefinition> {
        self.all_tools().into_iter().find(|tool| tool.name == name)
    }

    pub fn tool_description(&self, request: usize, name: &str) -> String {
        self.tools_by_turn.lock().expect("captured tools")[request]
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("request {request} should expose tool {name}"))
            .description
            .clone()
    }

    pub fn tool_context(&self) -> String {
        let turns = self.tools_by_turn.lock().expect("captured tools").clone();
        if turns.is_empty() {
            return "captured provider tools: no provider requests captured".to_owned();
        }
        let lines = turns
            .iter()
            .enumerate()
            .map(|(index, tools)| {
                let names = tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("turn {index}: {names}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("captured provider tools:\n{lines}")
    }
}

impl<P: LlmStream> LlmStream for CapturingProvider<P> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        self.inner.project_request(request)
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .push(request.tools.clone());
        self.request_prompts
            .lock()
            .expect("captured request prompts")
            .push(request.prompt.to_owned());
        self.system_prompts
            .lock()
            .expect("captured system prompts")
            .push(request.system_prompt.map(str::to_owned));
        self.inner.stream_request(request)
    }
}
