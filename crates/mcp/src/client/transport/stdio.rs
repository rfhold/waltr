use std::{process::Stdio, sync::Arc};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::Mutex,
};

use crate::{
    Error, Result,
    protocol::parse_json_rpc_str,
    trace::{request_id, request_method},
};

use super::{InboundSender, TransportInbound};

/// Stdio subprocess transport.
pub struct StdioTransport {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
}

impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StdioTransport")
            .finish_non_exhaustive()
    }
}

impl StdioTransport {
    /// Start a subprocess transport.
    pub async fn start(
        server_name: String,
        command: String,
        args: Vec<String>,
        inbound: InboundSender,
    ) -> Result<Self> {
        let mut child = Command::new(&command)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::protocol("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::protocol("missing stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => tracing::debug!(
                            mcp.server = %server_name,
                            mcp.transport = "stdio",
                            mcp.source = "stderr",
                            stream = "stderr",
                            line,
                            "mcp stderr"
                        ),
                        Ok(None) => break,
                        Err(error) => {
                            tracing::debug!(
                                mcp.server = %server_name,
                                mcp.transport = "stdio",
                                mcp.source = "stderr",
                                stream = "stderr",
                                %error,
                                "mcp stderr closed"
                            );
                            break;
                        }
                    }
                }
            });
        }
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match parse_json_rpc_str(&line) {
                        Ok(message) => drop(inbound.send(TransportInbound::Message(message))),
                        Err(error) => drop(inbound.send(TransportInbound::Disconnected(format!(
                            "invalid MCP stdio JSON-RPC message: {error}"
                        )))),
                    },
                    Ok(None) => {
                        drop(inbound.send(TransportInbound::Disconnected(
                            "MCP stdio server closed stdout".to_owned(),
                        )));
                        break;
                    }
                    Err(error) => {
                        drop(inbound.send(TransportInbound::Disconnected(format!(
                            "failed to read MCP stdio stdout: {error}"
                        ))));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
        })
    }

    /// Send one JSON-RPC message.
    pub async fn send(&self, message: Value) -> Result<()> {
        let method = request_method(&message).unwrap_or("");
        let id = request_id(&message);
        tracing::trace!(
            rpc.system = "jsonrpc",
            rpc.jsonrpc.version = "2.0",
            rpc.method = %method,
            mcp.method = %method,
            mcp.request_id = id.as_deref().unwrap_or(""),
            mcp.transport = "stdio",
            "sending MCP stdio message"
        );
        let line = serde_json::to_string(&message)?;
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Kill and wait for the subprocess.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        drop(child.start_kill());
        drop(child.wait().await);
    }
}
