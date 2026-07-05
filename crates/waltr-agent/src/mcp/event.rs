use mcp::McpLogLevel;
use serde_json::Value;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum McpRuntimeEvent {
    ServerInitialized {
        server_key: String,
    },
    ServerDisconnected {
        server_key: String,
        error: String,
    },
    ToolsChanged {
        server_key: String,
        generation: u64,
    },
    LogMessage {
        server_key: String,
        level: McpLogLevel,
        logger: Option<String>,
        data: Value,
    },
    Progress {
        server_key: String,
        token: Value,
        progress: Option<f64>,
        total: Option<f64>,
        message: Option<String>,
    },
}
