use std::{process::Stdio, sync::Arc};

use serde_json::Value;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
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
    stdin: Arc<Mutex<Option<ChildStdin>>>,
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
    ///
    /// # Errors
    ///
    /// Returns an error when the subprocess or its standard streams cannot be started.
    // Keep startup async to match the transport constructor API used by callers.
    #[allow(clippy::unused_async)]
    pub async fn start(
        server_name: String,
        command: String,
        args: Vec<String>,
        inbound: InboundSender,
        max_message_bytes: usize,
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
        tokio::spawn(read_stdout(stdout, inbound, max_message_bytes));
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(Some(stdin))),
        })
    }

    /// Send one JSON-RPC message.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization or writing to subprocess stdin fails.
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
        let mut stdin_guard = self.stdin.lock().await;
        let stdin = stdin_guard
            .as_mut()
            .ok_or_else(|| Error::protocol("MCP stdio transport is shut down"))?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        drop(stdin_guard);
        Ok(())
    }

    /// Close stdin, then wait for or terminate the subprocess.
    ///
    /// # Errors
    ///
    /// Returns an error when the subprocess cannot be terminated or reaped.
    pub async fn shutdown(&self) -> Result<()> {
        drop(self.stdin.lock().await.take());
        let mut child = self.child.lock().await;
        if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            child.start_kill()?;
            child.wait().await?;
        }
        drop(child);
        Ok(())
    }
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    inbound: InboundSender,
    max_message_bytes: usize,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_line_bounded(&mut reader, max_message_bytes).await {
            Ok(None) => {
                drop(inbound.send(TransportInbound::Disconnected(
                    "MCP stdio server closed stdout".to_owned(),
                )));
                break;
            }
            Ok(Some(line)) => {
                let line = match std::str::from_utf8(&line) {
                    Ok(line) => line,
                    Err(error) => {
                        drop(inbound.send(TransportInbound::Disconnected(format!(
                            "invalid UTF-8 in MCP stdio message: {error}"
                        ))));
                        break;
                    }
                };
                match parse_json_rpc_str(line) {
                    Ok(message) => drop(inbound.send(TransportInbound::Message(message))),
                    Err(error) => drop(inbound.send(TransportInbound::Disconnected(format!(
                        "invalid MCP stdio JSON-RPC message: {error}"
                    )))),
                }
            }
            Err(error) => {
                drop(inbound.send(TransportInbound::Disconnected(format!(
                    "failed to read MCP stdio stdout: {error}"
                ))));
                break;
            }
        }
    }
}

async fn read_line_bounded<R>(
    reader: &mut R,
    max_message_bytes: usize,
) -> std::io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if line.len().saturating_add(newline) > max_message_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "MCP stdio message exceeds the configured limit",
                ));
            }
            line.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
        if line.len().saturating_add(available.len()) > max_message_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP stdio message exceeds the configured limit",
            ));
        }
        let consumed = available.len();
        line.extend_from_slice(available);
        reader.consume(consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_line_reader_accepts_limit_and_rejects_overflow() {
        let mut accepted = BufReader::new(&b"1234\nrest"[..]);
        assert_eq!(
            read_line_bounded(&mut accepted, 4).await.unwrap(),
            Some(b"1234".to_vec())
        );

        let mut rejected = BufReader::with_capacity(2, &b"12345\n"[..]);
        let error = read_line_bounded(&mut rejected, 4).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
