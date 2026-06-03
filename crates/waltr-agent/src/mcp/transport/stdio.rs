use std::process::Stdio;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::Result;

use super::super::protocol::{JsonRpcMessage, json_rpc_result, protocol_error};

pub(in crate::mcp) struct StdioTransport {
    command: String,
    args: Vec<String>,
    connection: Mutex<Option<StdioConnection>>,
}

struct StdioConnection {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl StdioTransport {
    pub(in crate::mcp) fn new(command: String, args: Vec<String>) -> Self {
        Self {
            command,
            args,
            connection: Mutex::new(None),
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    pub(in crate::mcp) async fn request(&self, request: Value, id: Option<u64>) -> Result<Value> {
        let mut connection = self.connection.lock().await;
        if connection.is_none() {
            *connection = Some(self.spawn()?);
        }
        let connection = connection.as_mut().expect("stdio connection initialized");
        let line = serde_json::to_string(&request)?;
        connection.stdin.write_all(line.as_bytes()).await?;
        connection.stdin.write_all(b"\n").await?;
        connection.stdin.flush().await?;
        let Some(id) = id else {
            return Ok(json!(null));
        };
        loop {
            let Some(line) = connection.stdout.next_line().await? else {
                return Err(protocol_error("MCP stdio server closed stdout"));
            };
            let message: JsonRpcMessage = serde_json::from_str(&line)?;
            if message.id == Some(id) {
                return json_rpc_result(message);
            }
        }
    }

    fn spawn(&self) -> Result<StdioConnection> {
        let mut child = Command::new(&self.command)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| protocol_error("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| protocol_error("missing stdout"))?;
        Ok(StdioConnection {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
        })
    }
}
