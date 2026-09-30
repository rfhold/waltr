//! Embedded server composition and admission contracts.

use waltr::grpc::waltr::resource::v1::{
    InspectResourceRequest, ReplaceResourceRequest, resource_service_client::ResourceServiceClient,
};
use waltr_server::{
    LocalBearer,
    embedded::{
        EmbeddedCatalogFallback, EmbeddedCatalogSource, EmbeddedOutboundPolicy,
        EmbeddedServerConfig, build_embedded_server,
    },
};

fn config(root: &std::path::Path, bearer: &str) -> EmbeddedServerConfig {
    EmbeddedServerConfig {
        resources_root: root.join("resources"),
        waltr_database_path: root.join("data/waltr.sqlite3"),
        mcp_database_path: root.join("data/mcp.sqlite3"),
        session_root: root.join("sessions"),
        workspace_root: root.join("workspace"),
        catalog_source: EmbeddedCatalogSource::Path(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/models-dev-defaults.json"),
        ),
        catalog_cache_path: root.join("cache/models.json"),
        catalog_fallback: None,
        default_agent: "coding".to_owned(),
        local_bearer: LocalBearer::new(bearer).unwrap(),
        outbound_policy: EmbeddedOutboundPolicy::default(),
    }
}

#[tokio::test]
async fn embedded_server_rejects_relative_workspace_root() {
    let root = waltr_server::test_support::temp_root();
    let mut config = config(&root, "one-generation-secret");
    config.workspace_root = "relative-workspace".into();
    let error = build_embedded_server(config).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "embedded server paths are invalid or unavailable"
    );
}

#[tokio::test]
async fn embedded_server_requires_a_workspace_distinct_from_session_storage() {
    let root = waltr_server::test_support::temp_root();
    let mut config = config(&root, "one-generation-secret");
    config.workspace_root = config.session_root.clone();
    assert!(matches!(
        build_embedded_server(config).await,
        Err(waltr_server::embedded::EmbeddedBuildError::Paths)
    ));
}

#[tokio::test]
async fn embedded_rejects_nested_storage_and_host_files_before_creating_resources() {
    let root = waltr_server::test_support::temp_root();
    let original = config(&root, "one-generation-secret");
    let cases = [
        (root.join("sessions/worker"), original.session_root.clone()),
        (root.join("worker"), root.join("worker/sessions")),
        (root.join("data"), original.session_root.clone()),
        (root.join("cache"), original.session_root.clone()),
        (
            root.join("resources/.waltr/resources"),
            original.session_root.clone(),
        ),
    ];
    for (workspace, sessions) in cases {
        let mut candidate = original.clone();
        candidate.workspace_root = workspace;
        candidate.session_root = sessions;
        assert!(matches!(
            build_embedded_server(candidate).await,
            Err(waltr_server::embedded::EmbeddedBuildError::Paths)
        ));
        assert!(!original.resources_root.exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn embedded_rejects_configured_path_aliases_of_workspace_and_host_files() {
    use std::os::unix::fs::symlink;
    let root = waltr_server::test_support::temp_root();
    let original = config(&root, "one-generation-secret");
    std::fs::create_dir(root.join("workspace")).unwrap();
    symlink(root.join("workspace"), root.join("alias")).unwrap();
    let mut candidate = original.clone();
    candidate.session_root = root.join("alias/future/sessions");
    assert!(matches!(
        build_embedded_server(candidate).await,
        Err(waltr_server::embedded::EmbeddedBuildError::Paths)
    ));

    let mut candidate = original.clone();
    candidate.waltr_database_path = root.join("alias/future/waltr.sqlite3");
    assert!(matches!(
        build_embedded_server(candidate).await,
        Err(waltr_server::embedded::EmbeddedBuildError::Paths)
    ));

    let mut candidate = original.clone();
    candidate.mcp_database_path = root.join("alias/future/mcp.sqlite3");
    assert!(matches!(
        build_embedded_server(candidate).await,
        Err(waltr_server::embedded::EmbeddedBuildError::Paths)
    ));

    let mut candidate = original.clone();
    candidate.catalog_cache_path = root.join("alias/future/models.json");
    assert!(matches!(
        build_embedded_server(candidate).await,
        Err(waltr_server::embedded::EmbeddedBuildError::Paths)
    ));

    let mut candidate = original.clone();
    candidate.workspace_root = root.join("alias");
    candidate.session_root = root.join("workspace/sessions");
    assert!(matches!(
        build_embedded_server(candidate).await,
        Err(waltr_server::embedded::EmbeddedBuildError::Paths)
    ));

    let mut candidate = original.clone();
    std::fs::create_dir_all(root.join("resources/.waltr")).unwrap();
    symlink(root.join("resources/.waltr"), root.join("config-alias")).unwrap();
    candidate.workspace_root = root.join("config-alias/workspace");
    assert!(matches!(
        build_embedded_server(candidate).await,
        Err(waltr_server::embedded::EmbeddedBuildError::Paths)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn embedded_startup_does_not_crawl_workspace_symlinks() {
    use std::os::unix::fs::symlink;

    let root = waltr_server::test_support::temp_root();
    let config = config(&root, "one-generation-secret");
    std::fs::create_dir(&config.workspace_root).unwrap();
    std::fs::create_dir(&config.session_root).unwrap();
    symlink(
        &config.session_root,
        config.workspace_root.join("journal-alias"),
    )
    .unwrap();
    symlink(root.join("missing"), config.workspace_root.join("dangling")).unwrap();

    let server = build_embedded_server(config).await.unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn embedded_server_materializes_bundled_resources_from_explicit_builtin_catalog() {
    let seed: serde_json::Value =
        serde_json::from_str(include_str!("../resources/models-dev-defaults.json"))
            .expect("production seed must remain valid models.dev JSON");
    for synthetic in [
        "claude-sonnet-4-5-20250929",
        "gpt-5.3-mini",
        "gpt-5.4-old",
        "gpt-5.10",
        "gpt-5.x",
        "gpt-6.3",
    ] {
        assert!(
            seed.as_object()
                .unwrap()
                .values()
                .all(|provider| provider["models"]
                    .as_object()
                    .is_some_and(|models| !models.contains_key(synthetic))),
            "production seed contains synthetic model {synthetic}"
        );
    }
    assert!(seed["openai"]["models"].get("gpt-5.6-sol").is_some());
    let mut anthropic_ids = seed["anthropic"]["models"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    anthropic_ids.sort_unstable();
    assert_eq!(
        anthropic_ids,
        [
            "claude-fable-5",
            "claude-haiku-4-5",
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-4-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
        ]
    );
    let mut openai_ids = seed["openai"]["models"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    openai_ids.sort_unstable();
    assert_eq!(openai_ids, ["gpt-5.2", "gpt-5.6", "gpt-5.6-sol"]);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let root = waltr_server::test_support::temp_root();
    let mut config = config(&root, "builtin-fallback-secret");
    config.catalog_source = EmbeddedCatalogSource::Url(format!("http://{address}/catalog"));
    config.catalog_fallback = Some(EmbeddedCatalogFallback::BuiltIn);
    assert!(!config.catalog_cache_path.exists());

    let server = build_embedded_server(config)
        .await
        .expect("the builtin catalog should materialize every bundled resource");

    assert!(!root.join("cache/models.json").exists());
    server.shutdown().await;
    upstream.await.unwrap();
}

#[tokio::test]
async fn embedded_server_prebinds_and_admits_only_exact_local_bearer() {
    let root = waltr_server::test_support::temp_root();
    let config = config(&root, "one-generation-secret");
    let persisted_path = config
        .resources_root
        .join(".waltr/resources/mcp-servers/waltr.yaml");
    std::fs::create_dir_all(persisted_path.parent().unwrap()).unwrap();
    std::fs::write(
        &persisted_path,
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: stdio\n  command: stale-generation\n",
    )
    .unwrap();
    let config_debug = format!("{config:?}");
    assert!(!config_debug.contains(&root.display().to_string()));
    assert!(!config_debug.contains("one-generation-secret"));
    let server = build_embedded_server(config).await.unwrap();
    assert!(server.local_addr().ip().is_loopback());
    assert_ne!(server.local_addr().port(), 0);
    let server_debug = format!("{server:?}");
    assert!(!server_debug.contains(&server.local_addr().to_string()));
    tokio::time::timeout(std::time::Duration::from_secs(2), server.wait_until_ready())
        .await
        .expect("embedded server readiness");
    let address = server.local_addr();
    let expected_source = format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: \"http://{address}/mcp\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(&persisted_path).unwrap(),
        expected_source
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve_with_shutdown(async move {
        drop(shutdown_rx.await);
    }));
    let client = reqwest::Client::new();

    let health = client
        .get(format!("http://{address}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    let callback = client
        .get(format!("http://{address}/outbound-oauth/callback"))
        .send()
        .await
        .unwrap();
    assert_ne!(callback.status(), reqwest::StatusCode::UNAUTHORIZED);
    let metadata = client
        .get(format!("http://{address}/.well-known/oauth-client/waltr"))
        .send()
        .await
        .unwrap();
    assert_eq!(metadata.status(), reqwest::StatusCode::NOT_FOUND);

    for authorization in [None, Some("Bearer wrong-generation-secret")] {
        let mut request = client.post(format!(
            "http://{address}/waltr.status.v1.StatusService/GetStatus"
        ));
        if let Some(authorization) = authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
    let admitted = client
        .post(format!(
            "http://{address}/waltr.status.v1.StatusService/GetStatus"
        ))
        .header(
            reqwest::header::AUTHORIZATION,
            "Bearer one-generation-secret",
        )
        .send()
        .await
        .unwrap();
    assert_ne!(admitted.status(), reqwest::StatusCode::UNAUTHORIZED);

    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut resources = ResourceServiceClient::new(channel);
    let mut inspect = tonic::Request::new(InspectResourceRequest {
        path: Some("mcp-servers/waltr.yaml".to_owned()),
    });
    inspect.metadata_mut().insert(
        "authorization",
        "Bearer one-generation-secret".parse().unwrap(),
    );
    let inspection = resources
        .inspect_resource(inspect)
        .await
        .unwrap()
        .into_inner()
        .resources
        .remove(0);
    assert!(inspection.persisted);
    assert_eq!(
        inspection.persisted_source.as_deref(),
        Some(expected_source.as_str())
    );

    let replacement = "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: https://override.example/mcp\n";
    let mut replace = tonic::Request::new(ReplaceResourceRequest {
        path: "mcp-servers/waltr.yaml".to_owned(),
        source: replacement.to_owned(),
        expected_etag: inspection.persisted_etag.clone(),
    });
    replace.metadata_mut().insert(
        "authorization",
        "Bearer one-generation-secret".parse().unwrap(),
    );
    let replaced = resources
        .replace_resource(replace)
        .await
        .unwrap()
        .into_inner()
        .resource
        .unwrap();
    assert!(replaced.persisted);
    assert_eq!(replaced.persisted_source.as_deref(), Some(replacement));

    let mut stale = tonic::Request::new(ReplaceResourceRequest {
        path: "mcp-servers/waltr.yaml".to_owned(),
        source: expected_source,
        expected_etag: inspection.persisted_etag,
    });
    stale.metadata_mut().insert(
        "authorization",
        "Bearer one-generation-secret".parse().unwrap(),
    );
    let stale_error = resources.replace_resource(stale).await.unwrap_err();
    assert_eq!(stale_error.code(), tonic::Code::FailedPrecondition);

    let mcp_rejected = client
        .post(format!("http://{address}/mcp"))
        .send()
        .await
        .unwrap();
    assert_eq!(mcp_rejected.status(), reqwest::StatusCode::UNAUTHORIZED);
    let mcp_admitted = client
        .post(format!("http://{address}/mcp"))
        .header(
            reqwest::header::AUTHORIZATION,
            "Bearer one-generation-secret",
        )
        .send()
        .await
        .unwrap();
    assert_ne!(mcp_admitted.status(), reqwest::StatusCode::UNAUTHORIZED);

    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn sequential_embedded_generations_overwrite_the_persisted_endpoint() {
    let root = waltr_server::test_support::temp_root();
    let first = build_embedded_server(config(&root, "first-generation"))
        .await
        .unwrap();
    let first_address = first.local_addr();
    let persisted_path = root.join("resources/.waltr/resources/mcp-servers/waltr.yaml");
    let first_source = std::fs::read_to_string(&persisted_path).unwrap();
    assert!(first_source.contains(&format!("http://{first_address}/mcp")));
    first.shutdown().await;
    let reservation = std::net::TcpListener::bind(first_address).unwrap();

    let second = build_embedded_server(config(&root, "second-generation"))
        .await
        .unwrap();
    assert_ne!(first_address, second.local_addr());
    let second_source = std::fs::read_to_string(&persisted_path).unwrap();
    assert_ne!(second_source, first_source);
    assert!(second_source.contains(&format!("http://{}/mcp", second.local_addr())));

    second.shutdown().await;
    drop(reservation);
}
