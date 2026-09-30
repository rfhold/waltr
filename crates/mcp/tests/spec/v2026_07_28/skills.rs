use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use mcp::{
    SkillCatalog,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
    },
    skills::{SKILLS_EXTENSION, SkillCacheScope},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use super::super::support::{SpecServer, modern_params, post_json};

const MARKDOWN: &[u8] = b"---\nname: inspect\ndescription: Inspect bounded state.\ncustom: {retained: true}\n---\nRead the action reference.\n";

fn catalog() -> Arc<SkillCatalog> {
    Arc::new(
        SkillCatalog::new()
            .with_skill(
                "ops/inspect",
                &[("SKILL.md", MARKDOWN), ("data.bin", &[0, 255, 10])],
            )
            .unwrap(),
    )
}

struct SkillServer {
    catalog: Arc<SkillCatalog>,
    meta: Arc<Mutex<Vec<Value>>>,
    advertise: bool,
}

impl ServerHandler for SkillServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("skill-spec", "1")
    }
    fn capabilities(&self) -> ServerCapabilities {
        if self.advertise {
            ServerCapabilities::new().skills()
        } else {
            ServerCapabilities::new().resources()
        }
    }
    fn skill_catalog(&self) -> Option<Arc<SkillCatalog>> {
        Some(Arc::clone(&self.catalog))
    }
    fn list_skills(
        &self,
        cursor: Option<String>,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<mcp::McpSkillList>> {
        self.meta.lock().unwrap().push(
            context
                .meta_value("io.modelcontextprotocol/clientCapabilities")
                .cloned()
                .unwrap_or(Value::Null),
        );
        let mut result = self.catalog.list(cursor).unwrap();
        result.ttl_ms = 12345;
        result.cache_scope = SkillCacheScope::Public;
        result.meta = Some(serde_json::Map::from_iter([(
            "origin".into(),
            json!("retained"),
        )]));
        Box::pin(async move { Ok(result) })
    }
}

struct ClientHandler;
impl McpClientHandler for ClientHandler {}

struct DeniedResources {
    catalog: Arc<SkillCatalog>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl ServerHandler for DeniedResources {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("denied-resources", "1")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().skills()
    }

    fn skill_catalog(&self) -> Option<Arc<SkillCatalog>> {
        Some(Arc::clone(&self.catalog))
    }

    fn read_resource(
        &self,
        uri: String,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<mcp::McpResourceResult>> {
        self.seen.lock().unwrap().push(json!({
            "uri": uri,
            "id": context.request_id(),
            "authorization": context.meta_value("test/authorization"),
            "capabilities": context.client_capabilities(),
        }));
        Box::pin(async {
            Err(mcp::server::ServerError::invalid_params(
                "resource access denied",
            ))
        })
    }
}

#[tokio::test]
async fn catalog_reads_cannot_bypass_handler_authorization_on_either_transport() -> mcp::Result<()>
{
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(DeniedResources {
        catalog: catalog(),
        seen: Arc::clone(&seen),
    });
    let http = SpecServer::start(handler.clone()).await?;
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (read, write) = tokio::io::split(server);
    let task = tokio::spawn(mcp::server::serve_stream(handler, read, write));
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();

    for transport in ["http", "stdio"] {
        for file in ["SKILL.md", "data.bin"] {
            let uri = format!("skill://ops/inspect/{file}");
            let id = format!("{transport}-{file}");
            let mut params = modern_params(json!({"uri": uri}));
            params["_meta"]["test/authorization"] = json!("deny");
            let request =
                json!({"jsonrpc":"2.0", "id":id, "method":"resources/read", "params":params});
            let response = if transport == "http" {
                post_json(&http.endpoint(), request).await?.body
            } else {
                write.write_all(format!("{request}\n").as_bytes()).await?;
                let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
                    .await
                    .unwrap()?
                    .unwrap();
                serde_json::from_str(&line)?
            };
            assert_eq!(response["id"], id);
            assert_eq!(response["error"]["code"], -32602);
            assert_eq!(response["error"]["message"], "resource access denied");
            assert!(
                response.get("result").is_none(),
                "catalog bytes leaked: {response}"
            );
            assert_eq!(
                seen.lock().unwrap().last().unwrap(),
                &json!({"uri":uri, "id":id, "authorization":"deny", "capabilities":{}})
            );
        }
    }
    assert_eq!(seen.lock().unwrap().len(), 4);
    drop(write);
    drop(lines);
    task.await.unwrap()?;
    Ok(())
}

#[tokio::test]
async fn typed_client_pages_atomic_skills_without_prefetch() -> mcp::Result<()> {
    let mut catalog = SkillCatalog::new();
    for index in 0..35 {
        let name = format!("skill-{index:02}");
        let entry = format!("---\nname: {name}\ndescription: x\n---\n");
        catalog = catalog.with_skill(&format!("ops/{name}"), &[("SKILL.md", entry.as_bytes())])?;
    }
    let server = SpecServer::start(Arc::new(SkillServer {
        catalog: Arc::new(catalog),
        meta: Arc::default(),
        advertise: true,
    }))
    .await?;
    let client = McpClient::connect(
        "pages",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(ClientHandler),
        McpClientOptions::new("pages", "1", Duration::from_secs(2)).with_skills(),
        CancellationToken::new(),
    )
    .await?;
    let first = client.list_skills(None, CancellationToken::new()).await?;
    assert_eq!(first.skills.len(), 32);
    let second = client
        .list_skills(first.next_cursor.as_deref(), CancellationToken::new())
        .await?;
    assert_eq!(second.skills.len(), 3);
    assert!(second.next_cursor.is_none());
    assert!(first.skills.iter().all(
        |skill| matches!(&skill.resources, mcp::McpSkillResources::Files(files) if files.len() == 1)
    ));
    client.shutdown().await?;
    Ok(())
}

struct UntrustedSkillServer {
    result: mcp::McpSkillResult,
    duplicate: bool,
}
impl ServerHandler for UntrustedSkillServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("untrusted", "1")
    }
    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().skills()
    }
    fn list_skills(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<mcp::McpSkillList>> {
        let skill = self.result.skill.clone();
        let result = mcp::McpSkillList {
            result_type: mcp::skills::CompleteResult::Complete,
            skills: if self.duplicate {
                vec![skill.clone(), skill]
            } else {
                vec![skill]
            },
            next_cursor: None,
            ttl_ms: 0,
            cache_scope: SkillCacheScope::Private,
            meta: None,
        };
        Box::pin(async move { Ok(result) })
    }
    fn get_skill(
        &self,
        _uri: String,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<mcp::McpSkillResult>> {
        let result = self.result.clone();
        Box::pin(async move { Ok(result) })
    }
}

#[tokio::test]
async fn typed_client_rejects_untrusted_manifests_and_wrong_get_identity() -> mcp::Result<()> {
    for malformed in [false, true] {
        let mut result = catalog().get("skill://ops/inspect/SKILL.md").unwrap();
        if malformed {
            let mcp::McpSkillResources::Files(files) = &mut result.skill.resources else {
                unreachable!()
            };
            files[0].digest = "sha256:invalid".into();
        }
        let server = SpecServer::start(Arc::new(UntrustedSkillServer {
            result,
            duplicate: !malformed,
        }))
        .await?;
        let client = McpClient::connect(
            "invalid",
            McpTransportConfig::StreamableHttp {
                endpoint: server.endpoint(),
                bearer_token: None,
            },
            Arc::new(ClientHandler),
            McpClientOptions::new("invalid", "1", Duration::from_secs(2)).with_skills(),
            CancellationToken::new(),
        )
        .await?;
        assert!(
            client
                .list_skills(None, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            client
                .get_skill("skill://another/inspect/SKILL.md", CancellationToken::new())
                .await
                .is_err()
        );
        client.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn client_opt_in_and_cache_metadata_survive_skills_dispatch() -> mcp::Result<()> {
    let meta = Arc::new(Mutex::new(Vec::new()));
    let server = SpecServer::start(Arc::new(SkillServer {
        catalog: catalog(),
        meta: Arc::clone(&meta),
        advertise: true,
    }))
    .await?;
    for enabled in [false, true] {
        let options = McpClientOptions::new("skill-client", "1", Duration::from_secs(2));
        let client = McpClient::connect(
            "skills-spec",
            McpTransportConfig::StreamableHttp {
                endpoint: server.endpoint(),
                bearer_token: None,
            },
            Arc::new(ClientHandler),
            if enabled {
                options.with_skills()
            } else {
                options
            },
            CancellationToken::new(),
        )
        .await?;
        let result = client.list_skills(None, CancellationToken::new()).await;
        if !enabled {
            assert!(result.is_err());
        } else {
            let page = result?;
            assert_eq!(page.ttl_ms, 12345);
            assert_eq!(page.cache_scope, SkillCacheScope::Public);
            assert_eq!(page.meta.as_ref().unwrap()["origin"], "retained");
            assert_eq!(
                page.skills[0].frontmatter["custom"],
                json!({"retained": true})
            );
            let result = client
                .get_skill(&page.skills[0].uri, CancellationToken::new())
                .await?;
            assert_eq!(result.skill, page.skills[0]);
            let read = client
                .read_resource("skill://ops/inspect/data.bin", CancellationToken::new())
                .await?;
            assert_eq!(read.raw["contents"][0]["blob"], "AP8K");
            assert!(
                client
                    .get_skill("skill://ops/missing/SKILL.md", CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        client.shutdown().await?;
    }
    let meta = meta.lock().unwrap();
    assert_eq!(
        meta.len(),
        1,
        "default-off client must never send skills/list"
    );
    assert_eq!(meta[0]["extensions"][SKILLS_EXTENSION], json!({}));
    Ok(())
}

#[tokio::test]
async fn http_capability_gate_and_unknown_entry_errors() -> mcp::Result<()> {
    for advertised in [false, true] {
        let server = SpecServer::start(Arc::new(SkillServer {
            catalog: catalog(),
            meta: Arc::default(),
            advertise: advertised,
        }))
        .await?;
        let response = post_json(&server.endpoint(), json!({"jsonrpc":"2.0", "id":1, "method":"skills/get", "params":modern_params(json!({"uri":"skill://unknown/SKILL.md"}))})).await?;
        assert_eq!(
            response.body["error"]["code"],
            if advertised { -32602 } else { -32601 }
        );
        let discover = post_json(&server.endpoint(), json!({"jsonrpc":"2.0", "id":2, "method":"server/discover", "params":modern_params(json!({}))})).await?;
        assert_eq!(
            discover.body["result"]["capabilities"]["extensions"]
                .get(SKILLS_EXTENSION)
                .is_some(),
            advertised
        );
        let missing_meta = post_json(&server.endpoint(), json!({"jsonrpc":"2.0", "id":3, "method":"skills/get", "params":{"uri":"skill://ops/inspect/SKILL.md"}})).await?;
        assert_eq!(missing_meta.body["error"]["code"], -32602);
    }
    Ok(())
}

#[tokio::test]
async fn stdio_skills_and_exact_resources_share_dispatch() -> mcp::Result<()> {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (read, write) = tokio::io::split(server);
    let task = tokio::spawn(mcp::server::serve_stream(
        Arc::new(SkillServer {
            catalog: catalog(),
            meta: Arc::default(),
            advertise: true,
        }),
        read,
        write,
    ));
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    for (method, params, expected) in [
        (
            "skills/get",
            json!({"uri":"skill://ops/inspect/SKILL.md"}),
            "skill",
        ),
        (
            "resources/read",
            json!({"uri":"skill://ops/inspect/SKILL.md"}),
            "contents",
        ),
    ] {
        write.write_all(format!("{}\n", json!({"jsonrpc":"2.0", "id":1, "method":method, "params":modern_params(params)})).as_bytes()).await?;
        let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()?
            .unwrap();
        let response: Value = serde_json::from_str(&line)?;
        assert!(response["result"].get(expected).is_some(), "{response}");
        if method == "resources/read" {
            assert_eq!(
                response["result"]["contents"][0]["text"]
                    .as_str()
                    .unwrap()
                    .as_bytes(),
                MARKDOWN
            );
        }
    }
    drop(write);
    drop(lines);
    task.await.unwrap()?;
    Ok(())
}

#[cfg(feature = "macros")]
struct MacroSkills {
    catalog: Arc<SkillCatalog>,
}

#[cfg(feature = "macros")]
#[mcp::mcp_server(name = "embedded", skills = self.catalog)]
impl MacroSkills {
    #[resource(uri = "unrelated://data", definition = mcp::parse_resource_definition(&json!({"uri":"unrelated://data", "name":"data"})).unwrap())]
    fn resource(uri: String, _context: ServerContext) -> ServerResult<mcp::McpResourceResult> {
        Ok(mcp::McpResourceResult::text(uri, "text/plain", "unrelated"))
    }
}

#[tokio::test]
#[cfg(feature = "macros")]
async fn ordinary_macro_only_advertises_actual_skills_and_preserves_resources() -> mcp::Result<()> {
    for catalog in [Arc::new(SkillCatalog::new()), catalog()] {
        let handler = MacroSkills { catalog };
        assert_eq!(
            handler.capabilities().extensions.is_some(),
            !handler.catalog.is_empty()
        );
        let populated = !handler.catalog.is_empty();
        let server = SpecServer::start(Arc::new(handler)).await?;
        for (uri, field, expected) in [
            ("unrelated://data", "text", "unrelated"),
            ("skill://ops/inspect/data.bin", "blob", "AP8K"),
        ] {
            let response = post_json(&server.endpoint(), json!({"jsonrpc":"2.0", "id":1, "method":"resources/read", "params":modern_params(json!({"uri":uri}))})).await?;
            if uri == "unrelated://data" || populated {
                assert_eq!(response.body["result"]["contents"][0][field], expected);
            } else {
                assert!(response.body.get("error").is_some());
            }
        }
    }
    Ok(())
}

#[cfg(feature = "progressive")]
struct ProgressiveSkills {
    catalog: Arc<SkillCatalog>,
}

#[cfg(feature = "progressive")]
#[mcp::progressive_server(skills = self.catalog, tool(name = "exec", description = "Grouped actions"))]
impl ProgressiveSkills {
    /// Inspect bounded state.
    #[action(tool = "exec", name = "inspect")]
    fn inspect() -> ServerResult<Value> {
        Ok(json!({"ok":true}))
    }
}

#[cfg(feature = "progressive")]
#[tokio::test]
async fn progressive_catalog_advertisement_and_old_actions_fail() -> mcp::Result<()> {
    for catalog in [Arc::new(SkillCatalog::new()), catalog()] {
        let handler = ProgressiveSkills { catalog };
        assert_eq!(
            handler.capabilities().extensions.is_some(),
            !handler.catalog.is_empty()
        );
        assert_eq!(
            handler.capabilities().resources.is_some(),
            !handler.catalog.is_empty()
        );
        let populated = !handler.catalog.is_empty();
        let server = SpecServer::start(Arc::new(handler)).await?;
        for action in ["help", "help.inspect", "help.runbooks", "runbooks.load"] {
            let response = post_json(&server.endpoint(), json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":modern_params(json!({"name":"exec", "arguments":{"action":action}}))})).await?;
            assert_eq!(response.body["error"]["code"], -32602);
            assert!(
                !response.body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("call `help`")
            );
        }
        if populated {
            let response = post_json(&server.endpoint(), json!({"jsonrpc":"2.0", "id":1, "method":"resources/read", "params":modern_params(json!({"uri":"skill://ops/inspect/data.bin"}))})).await?;
            assert_eq!(response.body["result"]["contents"][0]["blob"], "AP8K");
        }
    }
    Ok(())
}
