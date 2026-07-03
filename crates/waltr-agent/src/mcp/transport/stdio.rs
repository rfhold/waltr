use std::{process::Stdio, sync::Arc};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::Mutex,
};

use crate::Result;

use super::{InboundSender, TransportInbound};
use crate::mcp::protocol::{parse_json_rpc_str, protocol_error};

pub(in crate::mcp) struct StdioTransport {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
}

impl StdioTransport {
    pub(in crate::mcp) async fn start(
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
            .ok_or_else(|| protocol_error("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| protocol_error("missing stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            let command = command.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => tracing::debug!(server = %command, line, "mcp stderr"),
                        Ok(None) => break,
                        Err(error) => {
                            tracing::debug!(server = %command, %error, "mcp stderr closed");
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

    pub(in crate::mcp) async fn send(&self, message: Value) -> Result<()> {
        let line = serde_json::to_string(&message)?;
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    pub(in crate::mcp) async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        drop(child.start_kill());
        drop(child.wait().await);
    }
}
