use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use mcp::{
    McpToolCall, McpToolResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Notify,
};

use super::super::support::modern_params;

struct StdioServer;

impl ServerHandler for StdioServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("stdio-spec", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
    }
}

#[tokio::test]
async fn stdio_reports_errors_and_continues_to_discovery() -> mcp::Result<()> {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task = tokio::spawn(mcp::server::serve_stream(
        Arc::new(StdioServer),
        server_read,
        server_write,
    ));
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut responses = BufReader::new(client_read).lines();

    client_write.write_all(b"{invalid\n").await?;
    assert_eq!(
        read_response(&mut responses).await?["error"]["code"],
        -32700
    );
    client_write.write_all(b"{}\n").await?;
    assert_eq!(
        read_response(&mut responses).await?["error"]["code"],
        -32600
    );
    client_write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\",\"params\":{}}\n")
        .await?;
    assert_eq!(
        read_response(&mut responses).await?["error"]["code"],
        -32602
    );

    let discover = json!({
        "jsonrpc": "2.0", "id": -7, "method": "server/discover",
        "params": modern_params(json!({}))
    });
    client_write
        .write_all(format!("{}\n", serde_json::to_string(&discover)?).as_bytes())
        .await?;
    let response = read_response(&mut responses).await?;
    assert_eq!(response["id"], -7);
    assert_eq!(
        response["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );

    drop(client_write);
    drop(responses);
    task.await.unwrap()?;
    Ok(())
}

struct CancellationServer {
    started: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

struct DropProbe(Arc<AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl ServerHandler for CancellationServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("stdio-cancel-spec", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn call_tool(
        &self,
        _call: McpToolCall,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        let started = Arc::clone(&self.started);
        let dropped = Arc::clone(&self.dropped);
        Box::pin(async move {
            let _probe = DropProbe(dropped);
            started.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn stdio_cancellation_stops_the_matching_request() -> mcp::Result<()> {
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task = tokio::spawn(mcp::server::serve_stream(
        Arc::new(CancellationServer {
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        }),
        server_read,
        server_write,
    ));
    let (client_read, mut client_write) = tokio::io::split(client);
    let request = json!({
        "jsonrpc": "2.0", "id": "active", "method": "tools/call",
        "params": modern_params(json!({ "name": "wait", "arguments": {} }))
    });
    client_write
        .write_all(format!("{}\n", serde_json::to_string(&request)?).as_bytes())
        .await?;
    started.notified().await;
    let cancel = json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": "active", "reason": "test" }
    });
    client_write
        .write_all(format!("{}\n", serde_json::to_string(&cancel)?).as_bytes())
        .await?;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled handler should be dropped");

    drop(client_write);
    drop(client_read);
    task.await.unwrap()?;
    Ok(())
}

async fn read_response(
    responses: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
) -> mcp::Result<Value> {
    let line = tokio::time::timeout(Duration::from_secs(1), responses.next_line())
        .await
        .map_err(|_| mcp::Error::protocol("timed out waiting for stdio response"))??
        .ok_or_else(|| mcp::Error::protocol("stdio response stream closed"))?;
    Ok(serde_json::from_str(&line)?)
}
