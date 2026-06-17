use std::{process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time,
};
use tokio_util::sync::CancellationToken;

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
    pub(in crate::mcp) async fn request(
        &self,
        request: Value,
        id: Option<u64>,
        timeout: Duration,
        cancellation_token: CancellationToken,
    ) -> Result<Value> {
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
        let mut connection = self.connection.lock().await;
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
        if connection.is_none() {
            *connection = Some(self.spawn()?);
        }
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
        let line = serde_json::to_string(&request)?;
        {
            let active = connection.as_mut().expect("stdio connection initialized");
            active.stdin.write_all(line.as_bytes()).await?;
            active.stdin.write_all(b"\n").await?;
            active.stdin.flush().await?;
        }
        let Some(id) = id else {
            return Ok(json!(null));
        };
        let timeout = time::sleep(timeout);
        tokio::pin!(timeout);
        loop {
            let line = tokio::select! {
                biased;

                () = cancellation_token.cancelled() => {
                    reset_connection(&mut connection).await;
                    return Err(protocol_error("MCP request canceled"));
                }
                () = &mut timeout => {
                    reset_connection(&mut connection).await;
                    return Err(protocol_error("MCP request timed out"));
                }
                line = connection.as_mut().expect("stdio connection initialized").stdout.next_line() => line?,
            };
            let Some(line) = line else {
                reset_connection(&mut connection).await;
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

async fn reset_connection(connection: &mut Option<StdioConnection>) {
    if let Some(mut connection) = connection.take() {
        drop(connection._child.start_kill());
        drop(connection._child.wait().await);
    }
}
