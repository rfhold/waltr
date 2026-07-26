//! Server access-authentication integration tests.

#![allow(clippy::match_same_arms, clippy::too_many_lines)]

use std::{pin::Pin, sync::OnceLock, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{Stream, StreamExt as _};

use waltr::grpc::waltr::{
    auth::v1::{
        AuthenticationDomain, AuthenticationFailure, AuthenticationFailureReason,
        ListAuthStatusesRequest, auth_service_client::AuthServiceClient,
    },
    status::v1::{WatchStatusRequest, status_service_client::StatusServiceClient},
};
use waltr_server::test_support::{
    AccessAuthTestServer, RestartableAccessAuthServer, setup_access_auth_browser_fixture,
    setup_access_auth_server, setup_server,
};
use prost::Message as _;
use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tonic::{Code, Request, transport::Endpoint};
use url::Url;

fn fixture() -> &'static AccessAuthTestServer {
    static SERVER: OnceLock<AccessAuthTestServer> = OnceLock::new();
    SERVER.get_or_init(setup_access_auth_server)
}

#[tokio::test]
async fn health_and_oidc_routes_are_public() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    assert_eq!(
        client
            .get(fixture.server.url("/health"))
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(fixture.server.url("/oidc/session"))
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    let login = client
        .get(
            fixture
                .server
                .url("/oidc/login?return_to=%2Fsessions%3Ftab%3Done"),
        )
        .send()
        .await?;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert!(
        login.headers()[header::LOCATION]
            .to_str()?
            .starts_with(&fixture.issuer)
    );

    Ok(())
}

#[cfg(feature = "web")]
#[tokio::test]
async fn web_navigation_redirects_with_encoded_local_return_path()
-> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .get(
            fixture()
                .server
                .url("/sessions/current?tab=one&filter=a%20b"),
        )
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/oidc/login?return_to=%2Fsessions%2Fcurrent%3Ftab%3Done%26filter%3Da%2520b"
    );
    Ok(())
}

#[cfg(feature = "web")]
#[tokio::test]
async fn browser_login_survives_reconstruction_and_logout_revokes_it()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let first_server = fixture.start_server(std::time::Duration::from_secs(3600));
    let client = no_redirect_client()?;

    let protected = client
        .get(first_server.url("/sessions/current?tab=one"))
        .send()
        .await?;
    assert_eq!(protected.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        protected.headers()[header::LOCATION],
        "/oidc/login?return_to=%2Fsessions%2Fcurrent%3Ftab%3Done"
    );

    let session_cookie = complete_browser_login(&client, &first_server, true).await?;
    assert_authenticated(&client, &first_server, &session_cookie).await?;
    first_server.stop();

    let second_server = fixture.start_server(std::time::Duration::from_secs(3600));
    assert_authenticated(&client, &second_server, &session_cookie).await?;
    let rejected_logout = client
        .post(second_server.url("/oidc/logout"))
        .header(header::COOKIE, &session_cookie)
        .send()
        .await?;
    assert_eq!(rejected_logout.status(), StatusCode::FORBIDDEN);
    assert!(!rejected_logout.headers().contains_key(header::SET_COOKIE));
    assert_authenticated(&client, &second_server, &session_cookie).await?;
    let origin = Url::parse(&second_server.url("/oidc/callback"))?
        .origin()
        .ascii_serialization();
    let logout = client
        .post(second_server.url("/oidc/logout"))
        .header(header::COOKIE, &session_cookie)
        .header(header::ORIGIN, origin)
        .header("sec-fetch-site", "same-origin")
        .send()
        .await?;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert!(
        cookie_from_response(&logout, "waltr-session-local")
            .is_some_and(|cookie| cookie == "waltr-session-local=")
    );

    let protected = client
        .get(second_server.url("/"))
        .header(header::COOKIE, &session_cookie)
        .send()
        .await?;
    assert_eq!(protected.status(), StatusCode::SEE_OTHER);
    let session = client
        .get(second_server.url("/oidc/session"))
        .header(header::COOKIE, &session_cookie)
        .send()
        .await?
        .json::<Value>()
        .await?;
    assert_eq!(session, json!({"authenticated": false}));
    Ok(())
}

#[tokio::test]
async fn browser_session_survives_same_configuration_reconstruction()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let first_server = fixture.start_server(std::time::Duration::from_hours(1));
    let client = no_redirect_client()?;
    let session_cookie = complete_browser_login(&client, &first_server, false).await?;
    assert_session_authenticated(&client, &first_server, &session_cookie).await?;
    first_server.stop();

    let second_server = fixture.start_server(std::time::Duration::from_hours(1));
    assert_session_authenticated(&client, &second_server, &session_cookie).await?;
    assert_eq!(fixture.revoked_browser_session_count().await, 0);
    Ok(())
}

#[tokio::test]
async fn changed_browser_client_rejects_and_revokes_old_session()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let first_server = fixture.start_server(std::time::Duration::from_hours(1));
    let client = no_redirect_client()?;
    let session_cookie = complete_browser_login(&client, &first_server, false).await?;
    first_server.stop();

    let second_server = fixture.start_server_with_client_id(
        std::time::Duration::from_hours(1),
        "different-browser-client",
    );
    assert_session_rejected(&client, &second_server, &session_cookie).await?;
    assert_eq!(fixture.revoked_browser_session_count().await, 1);
    Ok(())
}

#[tokio::test]
async fn changed_issuer_rejects_and_revokes_old_session() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = setup_access_auth_browser_fixture();
    let first_server = fixture.start_server(std::time::Duration::from_hours(1));
    let client = no_redirect_client()?;
    let session_cookie = complete_browser_login(&client, &first_server, false).await?;
    first_server.stop();

    let second_server = fixture.start_server_with_new_issuer(std::time::Duration::from_hours(1));
    assert_session_rejected(&client, &second_server, &session_cookie).await?;
    assert_eq!(fixture.revoked_browser_session_count().await, 1);
    Ok(())
}

#[tokio::test]
async fn browser_client_secret_rotation_preserves_session() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = setup_access_auth_browser_fixture();
    let first_server =
        fixture.start_server_with_client_secret(std::time::Duration::from_hours(1), "first-secret");
    let client = no_redirect_client()?;
    let session_cookie = complete_browser_login(&client, &first_server, false).await?;
    first_server.stop();

    let second_server = fixture
        .start_server_with_client_secret(std::time::Duration::from_hours(1), "rotated-secret");
    assert_session_authenticated(&client, &second_server, &session_cookie).await?;
    assert_eq!(fixture.revoked_browser_session_count().await, 0);
    Ok(())
}

#[cfg(feature = "web")]
#[tokio::test]
async fn expired_browser_session_cannot_access_protected_routes()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server(std::time::Duration::from_secs(1));
    let client = no_redirect_client()?;
    let session_cookie = complete_browser_login(&client, &server, false).await?;
    assert_authenticated(&client, &server, &session_cookie).await?;

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let response = client
        .get(server.url("/"))
        .header(header::COOKIE, session_cookie)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    Ok(())
}

#[cfg(feature = "web")]
#[tokio::test]
async fn callback_cleanup_failure_preserves_redirect_cookie_and_authenticated_session()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server =
        fixture.start_server_with_callback_cleanup_failure(std::time::Duration::from_secs(3600));
    let client = no_redirect_client()?;

    let session_cookie = complete_browser_login(&client, &server, false).await?;
    assert!(session_cookie.starts_with("waltr-session-local="));
    assert_authenticated(&client, &server, &session_cookie).await?;
    Ok(())
}

#[tokio::test]
async fn native_grpc_requires_a_valid_bearer_with_typed_access_details()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture();
    let channel = Endpoint::from_shared(fixture.server.base_url())?
        .connect()
        .await?;
    let mut client = StatusServiceClient::new(channel);

    let missing = client
        .watch_status(WatchStatusRequest::default())
        .await
        .expect_err("missing bearer should fail");
    assert_eq!(missing.code(), Code::Unauthenticated);
    assert_eq!(missing.message(), "Waltr access authentication required");
    assert_access_failure(&missing, AuthenticationFailureReason::CredentialRequired)?;

    let mut invalid = Request::new(WatchStatusRequest::default());
    invalid
        .metadata_mut()
        .insert("authorization", "Bearer invalid".parse()?);
    assert_eq!(
        client
            .watch_status(invalid)
            .await
            .expect_err("invalid bearer should fail")
            .code(),
        Code::Unauthenticated
    );

    let mut valid = Request::new(WatchStatusRequest::default());
    valid.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", fixture.access_token).parse()?,
    );
    let status = client
        .watch_status(valid)
        .await?
        .into_inner()
        .message()
        .await?
        .expect("status stream should emit");
    assert!(status.status.expect("status should be present").ready);
    Ok(())
}

#[tokio::test]
async fn grpc_web_requires_a_valid_browser_cookie() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture();
    let client = reqwest::Client::new();
    let endpoint = fixture
        .server
        .url("/waltr.status.v1.StatusService/WatchStatus");
    let request_body = [0_u8, 0, 0, 0, 0];

    let missing = client
        .post(&endpoint)
        .header(header::CONTENT_TYPE, "application/grpc-web+proto")
        .body(request_body.to_vec())
        .send()
        .await?;
    assert_eq!(missing.status(), StatusCode::OK);
    assert_eq!(
        missing.headers()[header::CONTENT_TYPE],
        "application/grpc-web+proto"
    );
    assert_eq!(missing.headers()["grpc-status"], "16");

    let bearer_only = client
        .post(&endpoint)
        .header(header::CONTENT_TYPE, "application/grpc-web+proto")
        .bearer_auth(&fixture.access_token)
        .body(request_body.to_vec())
        .send()
        .await?;
    assert_eq!(bearer_only.headers()["grpc-status"], "16");

    let valid = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/grpc-web+proto")
        .header(header::COOKIE, &fixture.session_cookie)
        .body(request_body.to_vec())
        .send()
        .await?;
    assert_eq!(valid.status(), StatusCode::OK);
    assert_eq!(
        valid.headers()[header::CONTENT_TYPE],
        "application/grpc-web+proto"
    );
    Ok(())
}

#[tokio::test]
async fn native_status_stream_ends_at_bearer_expiry() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server(Duration::from_hours(1));
    let channel = Endpoint::from_shared(server.base_url())?.connect().await?;
    let mut request = Request::new(WatchStatusRequest::default());
    let issued_at = std::time::Instant::now();
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", fixture.access_token(Duration::from_secs(2))).parse()?,
    );
    let mut stream = StatusServiceClient::new(channel.clone())
        .watch_status(request)
        .await?
        .into_inner();

    assert!(stream.message().await?.is_some());
    let status = tokio::time::timeout(Duration::from_secs(3), stream.message())
        .await?
        .expect_err("expired bearer stream should end with a status");
    assert_eq!(status.code(), Code::Unauthenticated);
    assert_eq!(status.message(), "Waltr access authentication required");
    assert_access_failure(&status, AuthenticationFailureReason::CredentialRejected)?;
    let elapsed = issued_at.elapsed();
    assert!(
        elapsed >= Duration::from_millis(900),
        "ended too early: {elapsed:?}"
    );
    assert!(
        elapsed <= Duration::from_millis(2_200),
        "ended too late: {elapsed:?}"
    );
    drop(stream);
    drop(channel);
    tokio::task::spawn_blocking(move || server.stop()).await?;
    Ok(())
}

#[tokio::test]
async fn grpc_web_status_stream_ends_at_browser_session_expiry()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server(Duration::from_secs(2));
    let client = no_redirect_client()?;
    let cookie = complete_browser_login(&client, &server, false).await?;
    let expires_at = fixture.browser_session_expiry(&cookie).await;
    let mut stream = grpc_web_watch(&client, &server, &cookie).await?;
    assert_grpc_web_initial_data(&mut stream).await?;

    let before_expiry = expires_at
        .duration_since(std::time::SystemTime::now())
        .unwrap_or(Duration::ZERO)
        .saturating_sub(Duration::from_millis(50));
    assert!(
        tokio::time::timeout(before_expiry, stream.next_frame())
            .await
            .is_err(),
        "browser stream terminated before its persisted expiry"
    );
    let remaining = expires_at
        .duration_since(std::time::SystemTime::now())
        .unwrap_or(Duration::ZERO);
    tokio::time::timeout(
        remaining + Duration::from_millis(300),
        assert_grpc_web_terminal_status(&mut stream, 16, "Waltr access authentication required"),
    )
    .await??;
    assert!(std::time::SystemTime::now() >= expires_at);
    drop(client);
    server.stop();
    Ok(())
}

#[tokio::test]
async fn same_instance_logout_immediately_ends_only_its_browser_stream()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server(Duration::from_hours(1));
    let client = no_redirect_client()?;
    let first_cookie = complete_browser_login(&client, &server, false).await?;
    let second_cookie = complete_browser_login(&client, &server, false).await?;
    let mut first = grpc_web_watch(&client, &server, &first_cookie).await?;
    let mut second = grpc_web_watch(&client, &server, &second_cookie).await?;
    assert_grpc_web_initial_data(&mut first).await?;
    assert_grpc_web_initial_data(&mut second).await?;

    logout_browser_session(&client, &server, &first_cookie).await?;
    tokio::time::timeout(
        Duration::from_millis(250),
        assert_grpc_web_terminal_status(&mut first, 16, "Waltr access authentication required"),
    )
    .await??;
    assert!(
        tokio::time::timeout(Duration::from_millis(1_100), second.next_frame())
            .await
            .is_err(),
        "logout of one session must not terminate another across a revalidation interval"
    );
    drop(first);
    drop(second);
    drop(client);
    tokio::task::spawn_blocking(move || server.stop()).await?;
    Ok(())
}

#[tokio::test]
async fn cross_instance_logout_ends_browser_stream_within_revalidation_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server_a = fixture.start_server(Duration::from_hours(1));
    let server_b = fixture.start_server(Duration::from_hours(1));
    let client = no_redirect_client()?;
    let cookie = complete_browser_login(&client, &server_a, false).await?;
    let mut stream = grpc_web_watch(&client, &server_b, &cookie).await?;
    assert_grpc_web_initial_data(&mut stream).await?;

    logout_browser_session(&client, &server_a, &cookie).await?;
    let revoked_at = std::time::Instant::now();
    tokio::time::timeout(
        Duration::from_millis(1_100),
        assert_grpc_web_terminal_status(&mut stream, 16, "Waltr access authentication required"),
    )
    .await??;
    assert!(revoked_at.elapsed() <= Duration::from_secs(1));
    drop(client);
    server_b.stop();
    server_a.stop();
    Ok(())
}

#[tokio::test]
async fn configuration_revocation_ends_existing_browser_stream()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let original = fixture.start_server(Duration::from_hours(1));
    let client = no_redirect_client()?;
    let cookie = complete_browser_login(&client, &original, false).await?;
    let mut stream = grpc_web_watch(&client, &original, &cookie).await?;
    assert_grpc_web_initial_data(&mut stream).await?;

    let changed_at = std::time::Instant::now();
    let reconfigured =
        fixture.start_server_with_client_id(Duration::from_hours(1), "replacement-client");
    tokio::time::timeout(
        Duration::from_millis(1_100),
        assert_grpc_web_terminal_status(&mut stream, 16, "Waltr access authentication required"),
    )
    .await??;
    assert!(changed_at.elapsed() <= Duration::from_secs(1));
    drop(client);
    reconfigured.stop();
    original.stop();
    Ok(())
}

#[tokio::test]
async fn browser_revalidation_store_failure_ends_stream_as_unavailable()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server(Duration::from_hours(1));
    let client = no_redirect_client()?;
    let cookie = complete_browser_login(&client, &server, false).await?;
    let mut stream = grpc_web_watch(&client, &server, &cookie).await?;
    assert_grpc_web_initial_data(&mut stream).await?;

    fixture.break_browser_session_revalidation().await;
    tokio::time::timeout(
        Duration::from_millis(1_100),
        assert_grpc_web_terminal_status(&mut stream, 14, "Waltr access authentication unavailable"),
    )
    .await??;
    drop(client);
    server.stop();
    Ok(())
}

#[tokio::test]
async fn disabled_mode_stream_remains_open_across_revalidation_intervals()
-> Result<(), Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(setup_server().base_url())?
        .connect()
        .await?;
    let mut stream = StatusServiceClient::new(channel)
        .watch_status(WatchStatusRequest::default())
        .await?
        .into_inner();
    assert!(stream.message().await?.is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(2_100), stream.message())
            .await
            .is_err(),
        "disabled-mode stream must remain open without a lease monitor"
    );
    Ok(())
}

#[tokio::test]
async fn provider_auth_service_is_behind_the_waltr_access_gate()
-> Result<(), Box<dyn std::error::Error>> {
    let channel = Endpoint::from_shared(fixture().server.base_url())?
        .connect()
        .await?;
    let status = AuthServiceClient::new(channel)
        .list_auth_statuses(ListAuthStatusesRequest::default())
        .await
        .expect_err("provider auth RPC should not be public");

    assert_eq!(status.code(), Code::Unauthenticated);
    assert_access_failure(&status, AuthenticationFailureReason::CredentialRequired)?;
    Ok(())
}

fn assert_access_failure(
    status: &tonic::Status,
    expected_reason: AuthenticationFailureReason,
) -> Result<(), prost::DecodeError> {
    let failure = AuthenticationFailure::decode(status.details())?;
    assert_eq!(failure.version, 1);
    assert_eq!(
        AuthenticationDomain::try_from(failure.domain),
        Ok(AuthenticationDomain::WaltrAccess)
    );
    assert_eq!(
        AuthenticationFailureReason::try_from(failure.reason),
        Ok(expected_reason)
    );
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hosted_mcp_metadata_and_access_fail_closed_at_the_http_boundary()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let client = no_redirect_client()?;
    let browser_cookie = fixture
        .browser_session_for_subject("mcp-boundary-subject")
        .await;
    let access_token = issue_mcp_access_token(&client, &server, &browser_cookie, "mcp:use").await?;
    let insufficient_scope_token =
        issue_mcp_access_token(&client, &server, &browser_cookie, "").await?;
    let upstream_access_token = fixture.access_token(Duration::from_hours(1));

    let api_metadata_response = client
        .get(server.url("/.well-known/oauth-protected-resource"))
        .send()
        .await?;
    assert_eq!(api_metadata_response.status(), StatusCode::OK);
    assert_eq!(
        api_metadata_response.headers()[header::CACHE_CONTROL],
        "no-store"
    );
    let api_metadata = api_metadata_response.json::<Value>().await?;
    let mcp_metadata = client
        .get(server.url("/.well-known/oauth-protected-resource/mcp"))
        .send()
        .await?
        .json::<Value>()
        .await?;
    assert_eq!(api_metadata["resource"], server.base_url());
    assert_ne!(api_metadata["resource"], mcp_metadata["resource"]);
    assert_ne!(
        api_metadata["authorization_servers"],
        mcp_metadata["authorization_servers"]
    );
    assert_eq!(mcp_metadata["resource"], "https://waltr.example/mcp");
    assert_eq!(
        mcp_metadata["authorization_servers"],
        json!(["https://waltr.example/oauth"])
    );
    assert_eq!(mcp_metadata["scopes_supported"], json!(["mcp:use"]));

    let endpoint = server.url("/mcp");
    let rejected_origin = client
        .post(&endpoint)
        .bearer_auth(&access_token)
        .header(header::ORIGIN, "https://browser.example")
        .header("accept", "application/json, text/event-stream")
        .json(&mcp_initialize_body())
        .send()
        .await?;
    assert_eq!(rejected_origin.status(), StatusCode::FORBIDDEN);

    let missing = mcp_post_raw(&client, &endpoint, None, None, b"not json").await?;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert!(
        missing.headers()[header::WWW_AUTHENTICATE]
            .to_str()?
            .contains("scope=\"mcp:use\"")
    );
    let invalid = mcp_post_raw(&client, &endpoint, Some("invalid"), None, b"not json").await?;
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
    assert!(
        invalid.headers()[header::WWW_AUTHENTICATE]
            .to_str()?
            .contains("error=\"invalid_token\"")
    );
    assert_eq!(
        mcp_post_raw(
            &client,
            &endpoint,
            Some(&upstream_access_token),
            None,
            b"not json",
        )
        .await?
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let insufficient = mcp_post_raw(
        &client,
        &endpoint,
        Some(&insufficient_scope_token),
        None,
        b"not json",
    )
    .await?;
    assert_eq!(insufficient.status(), StatusCode::FORBIDDEN);
    assert!(
        insufficient.headers()[header::WWW_AUTHENTICATE]
            .to_str()?
            .contains("error=\"insufficient_scope\"")
    );
    let cookie_only =
        mcp_post_raw(&client, &endpoint, None, Some(&browser_cookie), b"not json").await?;
    assert_eq!(cookie_only.status(), StatusCode::UNAUTHORIZED);

    let initialized = mcp_initialize(&client, &endpoint, &access_token).await?;
    assert_eq!(initialized.status(), StatusCode::OK);
    let session_id = initialized.headers()["mcp-session-id"].to_str()?.to_owned();
    drop(initialized.bytes().await?);
    assert_eq!(
        mcp_post(
            &client,
            &endpoint,
            &session_id,
            &access_token,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await?
        .status(),
        StatusCode::ACCEPTED
    );

    let rejected_delete = mcp_session_delete(&client, &endpoint, &session_id, "invalid").await?;
    assert_eq!(rejected_delete.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        mcp_session_get(&client, &endpoint, &session_id, &access_token)
            .await?
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        mcp_session_delete(&client, &endpoint, &session_id, &access_token)
            .await?
            .status(),
        StatusCode::ACCEPTED
    );
    server.stop();
    Ok(())
}

#[tokio::test]
async fn hosted_mcp_sessions_are_bound_to_the_oidc_subject()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let client = no_redirect_client()?;
    let first_cookie = fixture.browser_session_for_subject("first-subject").await;
    let second_cookie = fixture.browser_session_for_subject("second-subject").await;
    let first_token = issue_mcp_access_token(&client, &server, &first_cookie, "mcp:use").await?;
    let second_token = issue_mcp_access_token(&client, &server, &second_cookie, "mcp:use").await?;
    let endpoint = server.url("/mcp");

    let initialized = mcp_initialize(&client, &endpoint, &first_token).await?;
    assert_eq!(initialized.status(), StatusCode::OK);
    let session_id = initialized.headers()["mcp-session-id"].to_str()?.to_owned();
    drop(initialized.bytes().await?);
    assert_eq!(
        mcp_post(
            &client,
            &endpoint,
            &session_id,
            &second_token,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await?
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        mcp_post(
            &client,
            &endpoint,
            &session_id,
            &first_token,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await?
        .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        mcp_post(
            &client,
            &endpoint,
            &session_id,
            &second_token,
            json!({"jsonrpc": "2.0", "id": "inspect", "method": "ping"}),
        )
        .await?
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        mcp_session_get(&client, &endpoint, &session_id, &second_token)
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        mcp_session_delete(&client, &endpoint, &session_id, &second_token)
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        mcp_session_delete(&client, &endpoint, &session_id, &first_token)
            .await?
            .status(),
        StatusCode::ACCEPTED
    );
    server.stop();
    Ok(())
}

#[tokio::test]
async fn hosted_mcp_get_stream_ends_at_access_token_expiry()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let client = no_redirect_client()?;
    let browser_cookie = fixture
        .browser_session_for_subject("expiring-subject")
        .await;
    let access_token = issue_mcp_access_token(&client, &server, &browser_cookie, "mcp:use").await?;
    let access_token = fixture
        .mcp_access_token_with_ttl(&access_token, Duration::from_secs(5))
        .await;
    let endpoint = server.url("/mcp");
    let initialized = mcp_initialize(&client, &endpoint, &access_token).await?;
    assert_eq!(initialized.status(), StatusCode::OK);
    let session_id = initialized.headers()["mcp-session-id"].to_str()?.to_owned();
    drop(initialized.bytes().await?);
    assert_eq!(
        mcp_post(
            &client,
            &endpoint,
            &session_id,
            &access_token,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await?
        .status(),
        StatusCode::ACCEPTED
    );

    let stream = mcp_session_get(&client, &endpoint, &session_id, &access_token).await?;
    assert_eq!(stream.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(8), stream.bytes())
        .await
        .map_err(|_| "MCP GET stream survived hosted access-token expiry")??;
    server.stop();
    Ok(())
}

async fn issue_mcp_access_token(
    client: &reqwest::Client,
    server: &RestartableAccessAuthServer,
    browser_cookie: &str,
    scope: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    const CLIENT_ID: &str = "test-mcp-client";
    const REDIRECT_URI: &str = "https://client.example/callback";
    const RESOURCE: &str = "https://waltr.example/mcp";
    const VERIFIER: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let mut authorization = Url::parse(&server.url("/oauth/authorize"))?;
    authorization.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", scope),
        ("state", "access-auth-boundary"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    let consent = client
        .get(authorization)
        .header(header::COOKIE, browser_cookie)
        .send()
        .await?;
    assert_eq!(consent.status(), StatusCode::OK);
    let consent_cookie = response_cookie_prefix(&consent, "waltr-mcp-consent-local-")?;
    let consent_html = consent.text().await?;
    let transaction_id = hidden_value(&consent_html, "transaction_id")?;
    let csrf = hidden_value(&consent_html, "csrf")?;
    let approved = client
        .post(server.url("/oauth/authorize/decision"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(
            header::COOKIE,
            format!("{browser_cookie}; {consent_cookie}"),
        )
        .body(form(&[
            ("transaction_id", transaction_id),
            ("decision", "approve"),
            ("csrf", csrf),
        ]))
        .send()
        .await?;
    assert_eq!(approved.status(), StatusCode::SEE_OTHER);
    let redirect = Url::parse(approved.headers()[header::LOCATION].to_str()?)?;
    let code = redirect
        .query_pairs()
        .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
        .ok_or("authorization response omitted code")?;
    let issued = client
        .post(server.url("/oauth/token"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", VERIFIER),
            ("client_id", CLIENT_ID),
            ("resource", RESOURCE),
        ]))
        .send()
        .await?;
    assert_eq!(issued.status(), StatusCode::OK);
    let body = issued.json::<Value>().await?;
    body["access_token"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| "token response omitted access token".into())
}

async fn mcp_initialize(
    client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post(endpoint)
        .bearer_auth(access_token)
        .header("accept", "application/json, text/event-stream")
        .json(&mcp_initialize_body())
        .send()
        .await
}

fn mcp_initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": "initialize",
        "method": "initialize",
        "params": {
            "protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "access-auth-test", "version": "1.0.0"}
        }
    })
}

async fn mcp_post_raw(
    client: &reqwest::Client,
    endpoint: &str,
    bearer: Option<&str>,
    cookie: Option<&str>,
    body: &'static [u8],
) -> Result<reqwest::Response, reqwest::Error> {
    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    request.send().await
}

async fn mcp_post(
    client: &reqwest::Client,
    endpoint: &str,
    session_id: &str,
    access_token: &str,
    body: Value,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post(endpoint)
        .bearer_auth(access_token)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .json(&body)
        .send()
        .await
}

async fn mcp_session_get(
    client: &reqwest::Client,
    endpoint: &str,
    session_id: &str,
    access_token: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .get(endpoint)
        .bearer_auth(access_token)
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .send()
        .await
}

async fn mcp_session_delete(
    client: &reqwest::Client,
    endpoint: &str,
    session_id: &str,
    access_token: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .delete(endpoint)
        .bearer_auth(access_token)
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .send()
        .await
}

fn response_cookie_prefix(
    response: &reqwest::Response,
    prefix: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|cookie| cookie.starts_with(prefix))
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("response omitted cookie prefix {prefix}").into())
}

fn hidden_value<'a>(html: &'a str, name: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    let marker = format!("name=\"{name}\" value=\"");
    html.split_once(&marker)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(value, _)| value)
        .ok_or_else(|| format!("consent page omitted {name}").into())
}

fn form(values: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(values.iter().copied())
        .finish()
}

fn no_redirect_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

async fn grpc_web_watch(
    client: &reqwest::Client,
    server: &RestartableAccessAuthServer,
    session_cookie: &str,
) -> Result<GrpcWebStream, reqwest::Error> {
    let response = client
        .post(server.url("/waltr.status.v1.StatusService/WatchStatus"))
        .header(header::CONTENT_TYPE, "application/grpc-web+proto")
        .header(header::COOKIE, session_cookie)
        .body(vec![0_u8, 0, 0, 0, 0])
        .send()
        .await?;
    Ok(GrpcWebStream {
        body: Box::pin(response.bytes_stream()),
        buffer: Vec::new(),
        eof: false,
    })
}

async fn logout_browser_session(
    client: &reqwest::Client,
    server: &RestartableAccessAuthServer,
    session_cookie: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let origin = Url::parse(&server.url("/oidc/callback"))?
        .origin()
        .ascii_serialization();
    let response = client
        .post(server.url("/oidc/logout"))
        .header(header::COOKIE, session_cookie)
        .header(header::ORIGIN, origin)
        .header("sec-fetch-site", "same-origin")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    Ok(())
}

type GrpcWebBody =
    Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Send + Sync + 'static>>;

struct GrpcWebStream {
    body: GrpcWebBody,
    buffer: Vec<u8>,
    eof: bool,
}

enum GrpcWebFrame {
    Data(Vec<u8>),
    Trailers(String),
}

impl GrpcWebStream {
    async fn next_frame(&mut self) -> Result<Option<GrpcWebFrame>, Box<dyn std::error::Error>> {
        loop {
            if self.buffer.len() >= 5 {
                let kind = self.buffer[0];
                let length = u32::from_be_bytes(self.buffer[1..5].try_into()?) as usize;
                let end = 5_usize
                    .checked_add(length)
                    .ok_or("gRPC-web frame overflow")?;
                if self.buffer.len() >= end {
                    let frame = self.buffer.drain(..end).collect::<Vec<_>>();
                    return match kind {
                        0 => Ok(Some(GrpcWebFrame::Data(frame[5..].to_vec()))),
                        0x80 => Ok(Some(GrpcWebFrame::Trailers(
                            std::str::from_utf8(&frame[5..])?.to_owned(),
                        ))),
                        _ => Err(format!("unexpected gRPC-web frame kind {kind:#x}").into()),
                    };
                }
            }
            if self.eof {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                return Err("gRPC-web body ended between frames".into());
            }
            match self.body.next().await {
                Some(chunk) => self.buffer.extend_from_slice(&chunk?),
                None => self.eof = true,
            }
        }
    }
}

async fn assert_grpc_web_initial_data(
    stream: &mut GrpcWebStream,
) -> Result<(), Box<dyn std::error::Error>> {
    match stream.next_frame().await? {
        Some(GrpcWebFrame::Data(data)) => {
            assert!(
                !data.is_empty(),
                "initial protected data frame must not be empty"
            );
            Ok(())
        }
        Some(GrpcWebFrame::Trailers(_)) => Err("stream ended before protected data".into()),
        None => Err("stream ended before protected data".into()),
    }
}

async fn assert_grpc_web_terminal_status(
    stream: &mut GrpcWebStream,
    expected_status: u16,
    expected_message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut trailer_count = 0;
    let mut terminal_seen = false;
    while let Some(frame) = stream.next_frame().await? {
        match frame {
            GrpcWebFrame::Data(_) => {
                if terminal_seen {
                    return Err("gRPC-web data appeared after terminal status".into());
                }
                return Err("unexpected protected data before terminal status".into());
            }
            GrpcWebFrame::Trailers(trailers) => {
                trailer_count += 1;
                if terminal_seen {
                    return Err("gRPC-web stream emitted multiple trailer frames".into());
                }
                terminal_seen = true;
                assert_terminal_trailers(&trailers, expected_status, expected_message);
            }
        }
    }
    assert_eq!(trailer_count, 1, "expected exactly one trailer frame");
    Ok(())
}

fn assert_terminal_trailers(trailers: &str, expected_status: u16, expected_message: &str) {
    assert_eq!(trailers.matches("grpc-status:").count(), 1);
    assert!(trailers.contains(&format!("grpc-status:{expected_status}\r\n")));
    let encoded_message = expected_message.replace(' ', "%20");
    assert!(trailers.contains(&format!("grpc-message:{encoded_message}\r\n")));
    assert!(trailers.contains("grpc-status-details-bin"));
    assert!(!trailers.to_ascii_lowercase().contains("content-type:"));
}

async fn complete_browser_login(
    client: &reqwest::Client,
    server: &RestartableAccessAuthServer,
    check_correlation_failures: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let login = client
        .get(server.url("/oidc/login?return_to=%2F"))
        .send()
        .await?;
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    let correlation = cookie_from_response(&login, "waltr-login-local")
        .ok_or("login response did not set correlation cookie")?;
    let authorization_url = login.headers()[header::LOCATION].to_str()?.to_owned();
    let state = Url::parse(&authorization_url)?
        .query_pairs()
        .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
        .ok_or("authorization URL did not contain state")?;

    if check_correlation_failures {
        let missing = client
            .get(server.url(&format!("/oidc/callback?code=unused&state={state}")))
            .send()
            .await?;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        let mismatched = client
            .get(server.url(&format!("/oidc/callback?code=unused&state={state}")))
            .header(header::COOKIE, "waltr-login-local=wrong-correlation")
            .send()
            .await?;
        assert_eq!(mismatched.status(), StatusCode::UNAUTHORIZED);
    }

    let authorization = client.get(authorization_url).send().await?;
    assert_eq!(authorization.status(), StatusCode::SEE_OTHER);
    let callback_url = authorization.headers()[header::LOCATION].to_str()?;
    let callback = client
        .get(callback_url)
        .header(header::COOKIE, correlation)
        .send()
        .await?;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    assert_eq!(callback.headers()[header::LOCATION], "/");
    cookie_from_response(&callback, "waltr-session-local")
        .ok_or_else(|| "callback did not issue browser session cookie".into())
}

async fn assert_session_authenticated(
    client: &reqwest::Client,
    server: &RestartableAccessAuthServer,
    session_cookie: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = client
        .get(server.url("/oidc/session"))
        .header(header::COOKIE, session_cookie)
        .send()
        .await?
        .json::<Value>()
        .await?;
    assert_eq!(session, json!({"authenticated": true}));
    Ok(())
}

async fn assert_session_rejected(
    client: &reqwest::Client,
    server: &RestartableAccessAuthServer,
    session_cookie: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = client
        .get(server.url("/oidc/session"))
        .header(header::COOKIE, session_cookie)
        .send()
        .await?
        .json::<Value>()
        .await?;
    assert_eq!(session, json!({"authenticated": false}));
    Ok(())
}

#[cfg(feature = "web")]
async fn assert_authenticated(
    client: &reqwest::Client,
    server: &RestartableAccessAuthServer,
    session_cookie: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let protected = client
        .get(server.url("/"))
        .header(header::COOKIE, session_cookie)
        .send()
        .await?;
    assert_eq!(protected.status(), StatusCode::OK);
    let session = client
        .get(server.url("/oidc/session"))
        .header(header::COOKIE, session_cookie)
        .send()
        .await?
        .json::<Value>()
        .await?;
    assert_eq!(session, json!({"authenticated": true}));
    Ok(())
}

fn cookie_from_response(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|cookie| cookie.starts_with(&format!("{name}=")))
        .map(ToOwned::to_owned)
}
