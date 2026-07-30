//! Hosted MCP OAuth integration coverage.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use waltr_server::test_support::{setup_access_auth_browser_fixture, setup_server};
use reqwest::{StatusCode, header, redirect::Policy};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use url::Url;

const CLIENT_ID: &str = "test-mcp-client";
const CLIENT_REDIRECT: &str = "https://client.example/callback";
const RESOURCE: &str = "https://waltr.example/mcp";
const VERIFIER: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

#[tokio::test]
async fn dynamic_registration_requires_explicit_native_loopback_redirect_policy()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let denied_server = fixture.start_server_with_mcp_oauth_dcr(Duration::from_hours(1), false);
    let client = reqwest::Client::new();
    let request = json!({
        "redirect_uris": ["http://127.0.0.1:19876/mcp/oauth/callback"],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "client_name": "OpenCode",
        "client_uri": "https://opencode.ai",
        "scope": "mcp:use"
    });
    let denied = client
        .post(denied_server.url("/oauth/register"))
        .json(&request)
        .send()
        .await?;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);

    let enabled_server = fixture.start_server_with_mcp_oauth_dcr(Duration::from_hours(1), true);
    let registration = client
        .post(enabled_server.url("/oauth/register"))
        .json(&request)
        .send()
        .await?;
    assert_eq!(registration.status(), StatusCode::CREATED);
    let registration: Value = registration.json().await?;
    assert!(
        registration["client_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(
        registration["redirect_uris"],
        json!(["http://127.0.0.1:19876/mcp/oauth/callback"])
    );
    assert_eq!(registration["token_endpoint_auth_method"], "none");
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn browser_consent_issues_refreshes_and_revokes_local_mcp_tokens()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let replica = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()?;
    assert_eq!(
        client.get(server.url("/ready")).send().await?.status(),
        StatusCode::OK
    );

    let api_metadata: Value = client
        .get(server.url("/.well-known/oauth-protected-resource"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(api_metadata["resource"], server.base_url());
    assert_ne!(
        api_metadata["authorization_servers"][0],
        "https://waltr.example/oauth"
    );

    let mcp_metadata: Value = client
        .get(server.url("/.well-known/oauth-protected-resource/mcp"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(mcp_metadata["resource"], RESOURCE);
    assert_eq!(
        mcp_metadata["authorization_servers"],
        json!(["https://waltr.example/oauth"])
    );
    assert_eq!(mcp_metadata["scopes_supported"], json!(["mcp:use"]));

    let authorization_metadata = client
        .get(server.url("/.well-known/oauth-authorization-server/oauth"))
        .send()
        .await?;
    assert_eq!(authorization_metadata.status(), StatusCode::OK);
    let authorization_metadata: Value = authorization_metadata.json().await?;
    assert_eq!(
        authorization_metadata["issuer"],
        "https://waltr.example/oauth"
    );
    let jwks: Value = client
        .get(server.url("/oauth/jwks.json"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(jwks["keys"].as_array().map(Vec::len), Some(1));

    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let mut authorization = Url::parse(&server.url("/oauth/authorize"))?;
    authorization.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", CLIENT_REDIRECT),
        ("scope", "mcp:use"),
        ("state", "client-state"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    let start = client.get(authorization).send().await?;
    assert_eq!(start.status(), StatusCode::SEE_OTHER);
    let bridge_location = start.headers()[header::LOCATION].to_str()?.to_owned();
    assert!(bridge_location.starts_with("https://waltr.example/oauth/login?transaction_id="));
    assert!(!bridge_location.contains("client-state"));

    let bridge = client
        .get(replica.url(&local_path(&bridge_location)?))
        .send()
        .await?;
    assert_eq!(bridge.status(), StatusCode::SEE_OTHER);
    assert_eq!(bridge.headers()[header::REFERRER_POLICY], "no-referrer");
    let oauth_correlation = response_cookie(&bridge, "waltr-mcp-oauth-local")?;
    let oidc_start = client
        .get(replica.url(bridge.headers()[header::LOCATION].to_str()?))
        .send()
        .await?;
    assert_eq!(oidc_start.status(), StatusCode::SEE_OTHER);
    let oidc_correlation = response_cookie(&oidc_start, "waltr-login-local")?;
    let provider = client
        .get(oidc_start.headers()[header::LOCATION].to_str()?)
        .send()
        .await?;
    assert_eq!(provider.status(), StatusCode::SEE_OTHER);
    let callback = client
        .get(provider.headers()[header::LOCATION].to_str()?)
        .header(header::COOKIE, &oidc_correlation)
        .send()
        .await?;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    let browser_session = response_cookie(&callback, "waltr-session-local")?;
    let continuation = callback.headers()[header::LOCATION].to_str()?;
    assert!(continuation.starts_with("/oauth/authorize?transaction_id="));

    let missing_correlation = client
        .get(server.url(continuation))
        .header(header::COOKIE, &browser_session)
        .send()
        .await?;
    assert_eq!(missing_correlation.status(), StatusCode::BAD_REQUEST);
    let wrong_correlation = client
        .get(server.url(continuation))
        .header(
            header::COOKIE,
            format!("{browser_session}; waltr-mcp-oauth-local=wrong"),
        )
        .send()
        .await?;
    assert_eq!(wrong_correlation.status(), StatusCode::BAD_REQUEST);

    let consent = client
        .get(server.url(continuation))
        .header(
            header::COOKIE,
            format!("{browser_session}; {oauth_correlation}"),
        )
        .send()
        .await?;
    assert_eq!(consent.status(), StatusCode::OK);
    assert_eq!(consent.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(consent.headers()[header::REFERRER_POLICY], "no-referrer");
    let consent_cookie = response_cookie_prefix(&consent, "waltr-mcp-consent-local-")?;
    let consent_html = consent.text().await?;
    assert!(consent_html.contains("Test MCP Client"));
    assert!(consent_html.contains(CLIENT_ID));
    assert!(consent_html.contains(RESOURCE));
    assert!(consent_html.contains("mcp:use"));
    let transaction_id = hidden_value(&consent_html, "transaction_id")?;
    let csrf = hidden_value(&consent_html, "csrf")?;

    let replay = client
        .get(server.url(continuation))
        .header(
            header::COOKIE,
            format!("{browser_session}; {oauth_correlation}"),
        )
        .send()
        .await?;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);

    let rejected_csrf = client
        .post(server.url("/oauth/authorize/decision"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, &browser_session)
        .body(form(&[
            ("transaction_id", transaction_id),
            ("decision", "approve"),
            ("csrf", "wrong"),
        ]))
        .send()
        .await?;
    assert_eq!(rejected_csrf.status(), StatusCode::BAD_REQUEST);

    let approved = client
        .post(server.url("/oauth/authorize/decision"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(
            header::COOKIE,
            format!("{browser_session}; {consent_cookie}"),
        )
        .body(form(&[
            ("transaction_id", transaction_id),
            ("decision", "approve"),
            ("csrf", csrf),
        ]))
        .send()
        .await?;
    assert_eq!(approved.status(), StatusCode::SEE_OTHER);
    let approved_redirect = Url::parse(approved.headers()[header::LOCATION].to_str()?)?;
    assert_eq!(
        approved_redirect
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1,
        "client-state"
    );
    let code = approved_redirect
        .query_pairs()
        .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
        .ok_or("authorization response omitted code")?;

    let issued = client
        .post(server.url("/oauth/token"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", CLIENT_REDIRECT),
            ("code_verifier", VERIFIER),
            ("client_id", CLIENT_ID),
            ("resource", RESOURCE),
        ]))
        .send()
        .await?;
    assert_eq!(issued.status(), StatusCode::OK);
    let issued: Value = issued.json().await?;
    let access = issued["access_token"]
        .as_str()
        .ok_or("missing access token")?;
    let refresh = issued["refresh_token"]
        .as_str()
        .ok_or("missing refresh token")?;
    let principal = jwt_subject(access)?;

    assert_eq!(
        mcp_initialize(&client, &server.url("/mcp"), access)
            .await?
            .status(),
        StatusCode::OK
    );
    fixture.rotate_mcp_signing_key().await;
    let rotated_jwks: Value = client
        .get(replica.url("/oauth/jwks.json"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(rotated_jwks["keys"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        mcp_initialize(&client, &replica.url("/mcp"), access)
            .await?
            .status(),
        StatusCode::OK,
        "a one-hour token must remain valid across signing-key rotation"
    );
    let upstream = fixture.access_token(Duration::from_hours(1));
    assert_eq!(
        mcp_initialize(&client, &server.url("/mcp"), &upstream)
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let refreshed = client
        .post(server.url("/oauth/token"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT_ID),
            ("resource", RESOURCE),
        ]))
        .send()
        .await?;
    assert_eq!(refreshed.status(), StatusCode::OK);
    let refreshed: Value = refreshed.json().await?;
    let refreshed_access = refreshed["access_token"]
        .as_str()
        .ok_or("missing refreshed access token")?;
    assert_eq!(jwt_subject(refreshed_access)?, principal);

    let revoked = client
        .post(replica.url("/oauth/revoke"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[
            ("token", refreshed_access),
            ("client_id", CLIENT_ID),
        ]))
        .send()
        .await?;
    assert_eq!(revoked.status(), StatusCode::OK);
    assert_eq!(
        mcp_initialize(&client, &server.url("/mcp"), refreshed_access)
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );

    server.stop();
    replica.stop();
    Ok(())
}

#[tokio::test]
async fn disabled_auth_exposes_neither_oauth_routes_nor_mcp_challenges()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(server.url("/.well-known/oauth-authorization-server/oauth"))
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let initialized = client
        .post(server.url("/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&initialize_body())
        .send()
        .await?;
    assert_eq!(initialized.status(), StatusCode::OK);
    assert!(!initialized.headers().contains_key(header::WWW_AUTHENTICATE));
    Ok(())
}

#[tokio::test]
async fn enabled_access_without_hosted_oauth_fails_closed_for_mcp()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server(Duration::from_hours(1));
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(server.url("/mcp"))
            .json(&initialize_body())
            .send()
            .await?
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        client
            .get(server.url("/.well-known/oauth-protected-resource/mcp"))
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    server.stop();
    Ok(())
}

#[tokio::test]
async fn restart_preserves_the_active_signing_key_and_registered_client()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let client = reqwest::Client::new();
    let first = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let first_jwks: Value = client
        .get(first.url("/oauth/jwks.json"))
        .send()
        .await?
        .json()
        .await?;
    let key_id = first_jwks["keys"][0]["kid"]
        .as_str()
        .ok_or("first JWKS omitted key ID")?
        .to_owned();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
    let mut authorization = Url::parse(&first.url("/oauth/authorize"))?;
    authorization.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", CLIENT_REDIRECT),
        ("scope", "mcp:use"),
        ("state", "restart-state"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    let no_redirect = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()?;
    let started = no_redirect.get(authorization).send().await?;
    let bridge_location = started.headers()[header::LOCATION].to_str()?.to_owned();
    first.stop();

    let second = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let second_jwks: Value = client
        .get(second.url("/oauth/jwks.json"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(second_jwks["keys"][0]["kid"], key_id);

    let response = no_redirect
        .get(second.url(&local_path(&bridge_location)?))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(
        response.headers()[header::LOCATION]
            .to_str()?
            .starts_with("/oidc/login?return_to=")
    );
    second.stop();
    Ok(())
}

#[tokio::test]
async fn readiness_fails_closed_when_active_signing_public_key_mismatches_private_key()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = setup_access_auth_browser_fixture();
    let server = fixture.start_server_with_mcp_oauth(Duration::from_hours(1));
    let client = reqwest::Client::new();
    assert_eq!(
        client.get(server.url("/health")).send().await?.status(),
        StatusCode::OK
    );
    assert_eq!(
        client.get(server.url("/ready")).send().await?.status(),
        StatusCode::OK
    );

    fixture.corrupt_mcp_signing_public_key().await;

    assert_eq!(
        client.get(server.url("/health")).send().await?.status(),
        StatusCode::OK
    );
    assert_eq!(
        client.get(server.url("/ready")).send().await?.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    server.stop();
    Ok(())
}

async fn mcp_initialize(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post(endpoint)
        .bearer_auth(token)
        .header("accept", "application/json, text/event-stream")
        .json(&initialize_body())
        .send()
        .await
}

fn initialize_body() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": "initialize",
        "method": "initialize",
        "params": {
            "protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "oauth-server-test", "version": "1.0.0"}
        }
    })
}

fn response_cookie(
    response: &reqwest::Response,
    name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    response_cookie_prefix(response, &format!("{name}="))
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

fn jwt_subject(token: &str) -> Result<String, Box<dyn std::error::Error>> {
    let claims = token.split('.').nth(1).ok_or("invalid JWT")?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims)?)?;
    claims["sub"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| "JWT omitted subject".into())
}

fn local_path(url: &str) -> Result<String, url::ParseError> {
    let url = Url::parse(url)?;
    Ok(url[url::Position::BeforePath..].to_owned())
}
