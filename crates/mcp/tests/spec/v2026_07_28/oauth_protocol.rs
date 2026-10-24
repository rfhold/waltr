use std::collections::BTreeMap;

use mcp::{
    McpAccessToken, McpAuthorizationResponse, McpAuthorizationServerMetadata, McpBearerChallenge,
    McpClientMetadataDocument, McpClientRegistrationResponse, McpOAuthApplicationType,
    McpOAuthEndpointPolicy, McpOAuthGrantType, McpOAuthResponseType, McpOAuthSecret,
    McpPublicClientRegistrationRequest, McpTokenEndpointAuthMethod, McpTokenResponse,
    authorization_server_metadata_urls, protected_resource_metadata_urls,
};
use serde_json::json;

fn public_client_metadata(issuer: &str) -> McpAuthorizationServerMetadata {
    serde_json::from_value(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "revocation_endpoint": format!("{issuer}/revoke"),
        "registration_endpoint": format!("{issuer}/register"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "access_token_signing_alg_values_supported": ["ES256"],
        "authorization_response_iss_parameter_supported": true,
        "x-vendor": { "enabled": true }
    }))
    .expect("valid metadata")
}

fn web_registration() -> McpPublicClientRegistrationRequest {
    McpPublicClientRegistrationRequest {
        application_type: McpOAuthApplicationType::Web,
        redirect_uris: vec!["https://client.example/callback".to_owned()],
        grant_types: vec![
            McpOAuthGrantType::AuthorizationCode,
            McpOAuthGrantType::RefreshToken,
        ],
        response_types: vec![McpOAuthResponseType::Code],
        token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
        client_name: Some("Example client".to_owned()),
    }
}

#[test]
fn authorization_server_metadata_preserves_extensions_and_validates_public_policy() {
    let mut metadata = public_client_metadata("https://auth.example.com");
    assert_eq!(metadata.extra["x-vendor"], json!({ "enabled": true }));
    assert_eq!(
        serde_json::to_value(&metadata).expect("metadata")["x-vendor"],
        json!({ "enabled": true })
    );
    metadata.grant_types_supported.clear();
    metadata
        .validate_for_public_authorization_code_client(
            "https://auth.example.com",
            McpOAuthEndpointPolicy::HttpsOnly,
        )
        .expect("RFC default includes authorization_code");

    for mutate in [
        |value: &mut McpAuthorizationServerMetadata| value.response_types_supported.clear(),
        |value: &mut McpAuthorizationServerMetadata| {
            value.code_challenge_methods_supported.clear();
        },
        |value: &mut McpAuthorizationServerMetadata| {
            value.token_endpoint_auth_methods_supported.clear();
        },
        |value: &mut McpAuthorizationServerMetadata| value.token_endpoint = None,
    ] {
        let mut invalid = public_client_metadata("https://auth.example.com");
        mutate(&mut invalid);
        assert!(
            invalid
                .validate_for_public_authorization_code_client(
                    "https://auth.example.com",
                    McpOAuthEndpointPolicy::HttpsOnly,
                )
                .is_err()
        );
    }
    let mut preregistered = public_client_metadata("https://auth.example.com");
    preregistered.registration_endpoint = None;
    preregistered.client_id_metadata_document_supported = None;
    preregistered
        .validate_for_public_authorization_code_client(
            "https://auth.example.com",
            McpOAuthEndpointPolicy::HttpsOnly,
        )
        .expect("preregistered clients need no registration capability");
}

#[test]
fn authorization_metadata_enforces_issuer_endpoint_and_loopback_policy() {
    let metadata = public_client_metadata("https://auth.example.com");
    assert!(
        metadata
            .validate_for_public_authorization_code_client(
                "https://other.example.com",
                McpOAuthEndpointPolicy::HttpsOnly,
            )
            .is_err()
    );
    let mut loopback = public_client_metadata("http://127.0.0.1:8080");
    assert!(
        loopback
            .validate_for_public_authorization_code_client(
                "http://127.0.0.1:8080",
                McpOAuthEndpointPolicy::HttpsOnly,
            )
            .is_err()
    );
    loopback
        .validate_for_public_authorization_code_client(
            "http://127.0.0.1:8080",
            McpOAuthEndpointPolicy::AllowLoopbackHttp,
        )
        .expect("explicit loopback policy");
    loopback.token_endpoint = Some("https://user:secret@auth.example.com/token".to_owned());
    assert!(
        loopback
            .validate_for_public_authorization_code_client(
                "http://127.0.0.1:8080",
                McpOAuthEndpointPolicy::AllowLoopbackHttp,
            )
            .is_err()
    );
}

#[test]
fn authorization_response_parser_rejects_duplicates_mixed_and_unknown_outcomes() {
    for query in [
        "code=a&code=b&state=s",
        "code=a&error=access_denied&state=s",
        "code=a",
        "error=not_standard",
        "code=a&state=s&unexpected=true",
    ] {
        assert!(
            McpAuthorizationResponse::parse_query(query).is_err(),
            "{query}"
        );
    }
    assert!(
        McpAuthorizationResponse::parse_query("code=a&state=s&iss=https%3A%2F%2Fauth.example")
            .is_ok()
    );
}

#[test]
fn client_metadata_requires_exact_identity_public_profile_and_extensions() {
    let client_id = "https://client.example/metadata.json";
    let mut document = McpClientMetadataDocument {
        client_id: client_id.to_owned(),
        redirect_uris: vec!["https://client.example/callback".to_owned()],
        grant_types: vec![
            McpOAuthGrantType::AuthorizationCode,
            McpOAuthGrantType::RefreshToken,
        ],
        response_types: vec![McpOAuthResponseType::Code],
        token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
        client_name: Some("Example client".to_owned()),
        extra: BTreeMap::from([
            ("code_challenge_methods".to_owned(), json!(["S256"])),
            ("x-client-extension".to_owned(), json!(true)),
        ]),
    };
    document
        .validate_public_client(client_id, false)
        .expect("valid CIMD profile");
    assert!(
        document
            .validate_client_id("https://client.example/other")
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(&document).expect("document")["x-client-extension"],
        true
    );
    document
        .extra
        .insert("code_challenge_methods".to_owned(), json!(["plain"]));
    document.validate_public_client(client_id, false).unwrap();
    document.extra.remove("code_challenge_methods");
    document.validate_public_client(client_id, false).unwrap();
    let mut confidential = serde_json::to_value(&document).unwrap();
    confidential["token_endpoint_auth_method"] = json!("client_secret_basic");
    assert!(serde_json::from_value::<McpClientMetadataDocument>(confidential).is_err());
}

#[test]
fn opencode_client_metadata_needs_no_pkce_extension() {
    let client_id = "https://opencode.ai/oauth/opencode/client.json";
    let document: McpClientMetadataDocument = serde_json::from_value(json!({
        "client_id": client_id,
        "client_name": "OpenCode",
        "application_type": "native",
        "redirect_uris": ["http://127.0.0.1/callback", "http://localhost/callback"],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    }))
    .unwrap();
    document.validate_public_client(client_id, true).unwrap();
    assert!(document.validate_public_client(client_id, false).is_err());
}

#[test]
fn registration_normalizes_and_binds_application_type_to_redirect_policy() {
    let mut request = web_registration();
    request.redirect_uris.push(request.redirect_uris[0].clone());
    request.grant_types.reverse();
    let normalized = request
        .validate_and_normalize(false, 4, 256)
        .expect("valid web registration");
    assert_eq!(normalized.redirect_uris.len(), 1);
    assert_eq!(
        normalized.grant_types,
        [
            McpOAuthGrantType::AuthorizationCode,
            McpOAuthGrantType::RefreshToken
        ]
    );

    for redirect in [
        "http://client.example/callback",
        "https://user:password@client.example/callback",
        "https://*.example/callback",
        "https://client.example/callback#fragment",
    ] {
        let mut invalid = normalized.clone();
        invalid.redirect_uris = vec![redirect.to_owned()];
        assert!(invalid.validate_and_normalize(false, 4, 256).is_err());
    }
    let mut native = normalized;
    native.application_type = McpOAuthApplicationType::Native;
    native.redirect_uris = vec!["http://127.0.0.1:49152/callback".to_owned()];
    assert!(
        native
            .clone()
            .validate_and_normalize(false, 4, 256)
            .is_err()
    );
    assert!(native.validate_and_normalize(true, 4, 256).is_ok());
}

#[test]
fn registration_response_rejects_credentials_mismatches_and_redacts_secrets() {
    let request = web_registration();
    let mut response = McpClientRegistrationResponse {
        client_id: "opaque-client".to_owned(),
        client_secret: None,
        client_secret_expires_at: None,
        registration_access_token: None,
        registration_client_uri: None,
        client_id_issued_at: Some(1),
        application_type: Some(request.application_type),
        redirect_uris: request.redirect_uris.clone(),
        grant_types: request.grant_types.clone(),
        response_types: request.response_types.clone(),
        token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
        client_name: request.client_name.clone(),
    };
    response
        .validate_public_response(&request)
        .expect("public DCR response");
    response.registration_access_token = Some(McpOAuthSecret::new("registration-secret"));
    assert!(response.validate_public_response(&request).is_err());
    assert!(!format!("{response:?}").contains("registration-secret"));
    response.registration_access_token = None;
    response.application_type = Some(McpOAuthApplicationType::Native);
    assert!(response.validate_public_response(&request).is_err());
    response.application_type = None;
    response.validate_public_response(&request).unwrap();

    let valid = response;
    for (field, value) in [
        ("client_id", json!("")),
        ("client_secret", json!("client-secret")),
        ("client_secret_expires_at", json!(0)),
        ("registration_access_token", json!("registration-secret")),
        (
            "registration_client_uri",
            json!("https://auth.example/register/client"),
        ),
        ("redirect_uris", json!(["https://other.example/callback"])),
        ("grant_types", json!(["authorization_code"])),
        ("response_types", json!([])),
        ("client_name", json!("Other client")),
    ] {
        let mut invalid = serde_json::to_value(&valid).unwrap();
        invalid[field] = value;
        let invalid: McpClientRegistrationResponse = serde_json::from_value(invalid).unwrap();
        assert!(
            invalid.validate_public_response(&request).is_err(),
            "{field}"
        );
    }
    let mut confidential = serde_json::to_value(&valid).unwrap();
    confidential["token_endpoint_auth_method"] = json!("client_secret_basic");
    assert!(serde_json::from_value::<McpClientRegistrationResponse>(confidential).is_err());
}

#[test]
fn registration_response_accepts_omitted_application_type_without_inference() {
    for (application_type, redirect_uri) in [
        (
            McpOAuthApplicationType::Web,
            "https://client.example/callback",
        ),
        (
            McpOAuthApplicationType::Native,
            "http://127.0.0.1:49152/callback",
        ),
    ] {
        let mut request = web_registration();
        request.application_type = application_type;
        request.redirect_uris = vec![redirect_uri.to_owned()];
        let wire = json!({
            "client_id": "chikin-client",
            "client_id_issued_at": 1,
            "redirect_uris": request.redirect_uris,
            "grant_types": request.grant_types,
            "response_types": request.response_types,
            "token_endpoint_auth_method": "none",
            "client_name": request.client_name
        });
        let response: McpClientRegistrationResponse = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(response.application_type, None);
        response.validate_public_response(&request).unwrap();
        assert!(
            serde_json::to_value(&response)
                .unwrap()
                .get("application_type")
                .is_none()
        );

        let mut present = wire.clone();
        present["application_type"] = json!(application_type);
        let response: McpClientRegistrationResponse = serde_json::from_value(present).unwrap();
        response.validate_public_response(&request).unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap()["application_type"],
            json!(application_type)
        );

        let mut mismatched = wire.clone();
        mismatched["application_type"] = json!(match application_type {
            McpOAuthApplicationType::Web => McpOAuthApplicationType::Native,
            McpOAuthApplicationType::Native => McpOAuthApplicationType::Web,
        });
        let response: McpClientRegistrationResponse = serde_json::from_value(mismatched).unwrap();
        assert!(response.validate_public_response(&request).is_err());

        let mut null = wire.clone();
        null["application_type"] = serde_json::Value::Null;
        let response: McpClientRegistrationResponse = serde_json::from_value(null).unwrap();
        assert_eq!(response.application_type, None);
        response.validate_public_response(&request).unwrap();

        for invalid in [json!("unsupported"), json!(true), json!(42), json!({})] {
            let mut invalid_wire = wire.clone();
            invalid_wire["application_type"] = invalid;
            assert!(serde_json::from_value::<McpClientRegistrationResponse>(invalid_wire).is_err());
        }
    }
}

#[test]
fn oauth_secrets_and_access_tokens_are_redacted() {
    let response = McpTokenResponse {
        access_token: McpOAuthSecret::new("access-sensitive"),
        token_type: mcp::McpOAuthTokenType::Bearer,
        expires_in: Some(300),
        refresh_token: Some(McpOAuthSecret::new("refresh-sensitive")),
        scope: None,
    };
    let debug = format!("{response:?}");
    assert!(!debug.contains("access-sensitive"));
    assert!(!debug.contains("refresh-sensitive"));
    assert!(debug.contains("[REDACTED]"));
    assert!(
        !format!("{:?}", McpAccessToken("token-sensitive".to_owned())).contains("token-sensitive")
    );
}

#[test]
fn bearer_challenges_and_discovery_urls_are_strict_and_path_aware() {
    let challenge = McpBearerChallenge::parse(
        r#"Basic realm="other", Bearer realm="mcp", scope="files:read", x-vendor="yes""#,
    )
    .expect("Bearer challenge");
    assert_eq!(challenge.realm.as_deref(), Some("mcp"));
    assert_eq!(challenge.scope.as_deref(), Some("files:read"));
    assert_eq!(challenge.parameters["x-vendor"], "yes");
    assert!(McpBearerChallenge::parse("Bearer scope=one, scope=two").is_none());

    assert_eq!(
        protected_resource_metadata_urls("https://resource.example/tenant/mcp"),
        [
            "https://resource.example/.well-known/oauth-protected-resource/tenant/mcp",
            "https://resource.example/.well-known/oauth-protected-resource"
        ]
    );
    assert_eq!(
        authorization_server_metadata_urls("https://auth.example/tenant"),
        [
            "https://auth.example/.well-known/oauth-authorization-server/tenant",
            "https://auth.example/.well-known/openid-configuration/tenant",
            "https://auth.example/tenant/.well-known/openid-configuration"
        ]
    );
}
