use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::{
    Form, Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use p256::ecdsa::{Signature, SigningKey, signature::Signer as _};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use waltr::grpc::waltr::auth::v1::{
    BeginAuthRequest, CancelAuthRequest, CompleteAuthRequest, GetAuthAttemptStatusRequest,
    ListAuthStatusesRequest, LogoutRequest, RefreshAuthRequest,
    auth_service_server::AuthService as _,
};

use crate::{
    auth::AuthService,
    persistence::{DatabaseConfig, Persistence},
    resource_catalog::AgentDefinitionInspection,
    test_support::resource_state_with_overrides,
};

use super::*;

const DEVICE_SECRET: &str = "private-device-secret";
const REFRESH_SECRET: &str = "workspace-refresh-secret";
const KEY: &str = "export";
const BINDING_PATH: &str = "workspace-bindings/export.yaml";

struct Issuer {
    base: String,
    polls: AtomicUsize,
    starts: AtomicUsize,
    other_account: AtomicBool,
    advertise_device: AtomicBool,
    wrong_resource: AtomicBool,
    leak_device_code: AtomicBool,
}

impl Issuer {
    fn token(&self, subject: &str, expiry_seconds: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","typ":"at+jwt","kid":"fixture"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            json!({
                "iss": self.base,
                "sub": subject,
                "aud": "waltr-api",
                "exp": jsonwebtoken::get_current_timestamp() + expiry_seconds,
                "scope": "waltr:api",
            })
            .to_string(),
        );
        let payload = format!("{header}.{claims}");
        let signature: Signature = signing_key().sign(payload.as_bytes());
        format!("{payload}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }
}

fn signing_key() -> SigningKey {
    SigningKey::from_slice(&[7; 32]).unwrap()
}

async fn issuer() -> (Arc<Issuer>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = Arc::new(Issuer {
        base: format!("http://{}", listener.local_addr().unwrap()),
        polls: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        other_account: AtomicBool::new(false),
        advertise_device: AtomicBool::new(true),
        wrong_resource: AtomicBool::new(false),
        leak_device_code: AtomicBool::new(false),
    });
    let router = Router::new()
        .route("/.well-known/oauth-protected-resource", get(resource))
        .route("/.well-known/openid-configuration", get(metadata))
        .route("/jwks", get(jwks))
        .route("/device", post(device))
        .route("/token", post(token))
        .with_state(Arc::clone(&issuer));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (issuer, server)
}

async fn resource(State(issuer): State<Arc<Issuer>>) -> Json<Value> {
    let resource = if issuer.wrong_resource.load(Ordering::SeqCst) {
        format!("{}/other", issuer.base)
    } else {
        issuer.base.clone()
    };
    Json(json!({
        "resource": resource,
        "authorization_servers": [issuer.base],
        "scopes_supported": ["waltr:api"],
        "waltr_native_client": {
            "client_id": "waltr-native",
            "scopes": ["openid", "offline_access", "waltr:api"],
            "audience": "waltr-api",
        }
    }))
}

async fn metadata(State(issuer): State<Arc<Issuer>>) -> Json<Value> {
    let mut metadata = json!({
        "issuer": issuer.base,
        "authorization_endpoint": format!("{}/authorize", issuer.base),
        "token_endpoint": format!("{}/token", issuer.base),
        "jwks_uri": format!("{}/jwks", issuer.base),
        "access_token_signing_alg_values_supported": ["ES256"],
    });
    if issuer.advertise_device.load(Ordering::SeqCst) {
        metadata["device_authorization_endpoint"] = json!(format!("{}/device", issuer.base));
    }
    Json(metadata)
}

async fn jwks() -> Json<Value> {
    let key = signing_key().verifying_key().to_encoded_point(false);
    Json(json!({ "keys": [{
        "kid": "fixture",
        "kty": "EC",
        "alg": "ES256",
        "use": "sig",
        "crv": "P-256",
        "x": URL_SAFE_NO_PAD.encode(key.x().unwrap()),
        "y": URL_SAFE_NO_PAD.encode(key.y().unwrap()),
    }] }))
}

async fn device(
    State(issuer): State<Arc<Issuer>>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    issuer.starts.fetch_add(1, Ordering::SeqCst);
    assert_eq!(form.len(), 4);
    assert_eq!(form["client_id"], "waltr-native");
    assert_eq!(form["scope"], "openid offline_access waltr:api");
    assert_eq!(form["resource"], issuer.base);
    assert_eq!(form["audience"], "waltr-api");
    let url = if issuer.leak_device_code.load(Ordering::SeqCst) {
        format!("{}/verify?code=%70rivate-device-secret", issuer.base)
    } else {
        format!("{}/verify", issuer.base)
    };
    Json(json!({
        "device_code": DEVICE_SECRET,
        "user_code": "PAGE-CODE",
        "verification_uri": url,
        "expires_in": 65,
        "interval": 1,
    }))
}

async fn token(
    State(issuer): State<Arc<Issuer>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    assert_eq!(form["client_id"], "waltr-native");
    if form["grant_type"] == "refresh_token" {
        assert_eq!(form["refresh_token"], REFRESH_SECRET);
        assert_eq!(form.len(), 3);
        let subject = if issuer.other_account.load(Ordering::SeqCst) {
            "other"
        } else {
            "first"
        };
        return Json(json!({
            "access_token": issuer.token(subject, 600),
            "token_type": "Bearer",
            "expires_in": 5000,
        }))
        .into_response();
    }
    assert_eq!(form.len(), 3);
    assert_eq!(
        form["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(form["device_code"], DEVICE_SECRET);
    if issuer.polls.fetch_add(1, Ordering::SeqCst) == 0 {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "authorization_pending" })),
        )
            .into_response()
    } else {
        Json(json!({
            "access_token": issuer.token("first", 3600),
            "refresh_token": REFRESH_SECRET,
            "token_type": "Bearer",
            "expires_in": 120,
        }))
        .into_response()
    }
}

async fn service(issuer: &Issuer) -> Arc<AuthService> {
    let (resources, store) = setup(issuer).await;
    AuthService::test_workspace_binding(resources, store)
}

async fn setup(issuer: &Issuer) -> (AgentDefinitionState, Arc<OutboundOAuthStore>) {
    let resources = cold_catalog(issuer);
    let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
        .await
        .unwrap();
    (resources, persistence.outbound_oauth_store())
}

fn binding_yaml(endpoint: &str) -> String {
    format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: WorkspaceBinding\nmetadata:\n  name: {KEY}\nspec:\n  endpoint: {endpoint}\n  machineId: machine\n  exportRoot: /fixture\n",
    )
}

fn cold_catalog(issuer: &Issuer) -> AgentDefinitionState {
    resource_state_with_overrides(&[(BINDING_PATH, &binding_yaml(&issuer.base))])
}

async fn replace_binding(resources: &AgentDefinitionState, endpoint: &str) {
    replace_slot(resources, BINDING_PATH, binding_yaml(endpoint)).await;
}

async fn replace_slot(resources: &AgentDefinitionState, path: &str, source: String) {
    let AgentDefinitionInspection::Resource(current) = resources.inspect(Some(path)).unwrap()
    else {
        panic!("expected one workspace binding");
    };
    resources
        .replace(path, source, &current.persisted_etag)
        .await
        .unwrap();
}

fn begin_request(method: AuthMethod) -> tonic::Request<BeginAuthRequest> {
    tonic::Request::new(BeginAuthRequest {
        target: Some(proto_target(KEY)),
        method: method as i32,
    })
}

async fn complete_device(
    service: &AuthService,
    attempt_id: &str,
) -> waltr::grpc::waltr::auth::v1::GetAuthAttemptStatusResponse {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let response = service
                .get_auth_attempt_status(tonic::Request::new(GetAuthAttemptStatusRequest {
                    attempt_id: attempt_id.to_owned(),
                }))
                .await
                .unwrap()
                .into_inner();
            if response.attempt.as_ref().unwrap().lifecycle != AuthAttemptLifecycle::Pending as i32
            {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn workspace_binding_refresh_connection_failure_preserves_the_grant() {
    let (issuer, server) = issuer().await;
    let (resources, store) = setup(&issuer).await;
    let service = AuthService::test_workspace_binding(resources.clone(), Arc::clone(&store));
    let begin = service
        .begin_auth(begin_request(AuthMethod::OauthDevice))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        complete_device(&service, &begin.attempt_id)
            .await
            .attempt
            .unwrap()
            .lifecycle,
        AuthAttemptLifecycle::Succeeded as i32
    );

    let binding = WorkspaceBindingAuth::new(resources, store, OutboundHttpPolicy::loopback_test());
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut credential = binding.read(KEY).await.unwrap().unwrap().credential;
    let OAuthCredentialContext::WorkspaceBinding(context) = &mut credential.context else {
        panic!("expected a WorkspaceBinding grant");
    };
    context.token_endpoint =
        Url::parse(&format!("http://{}/token", socket.local_addr().unwrap())).unwrap();
    binding
        .credentials
        .write(&credential_key(KEY), credential)
        .await
        .unwrap();

    for _ in 0..2 {
        let error = binding.refresh(KEY).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unavailable);
        assert_eq!(
            error.message(),
            "workspace binding refresh connection failed"
        );
        assert!(
            !binding
                .read(KEY)
                .await
                .unwrap()
                .unwrap()
                .reauthorization_required
        );
    }
    server.abort();
}

#[tokio::test]
async fn workspace_binding_device_rpc_rotates_pinned_grant_and_resets_locally() {
    let (issuer, server) = issuer().await;
    let (resources, store) = setup(&issuer).await;
    replace_binding(&resources, "https://other.example/").await;
    replace_binding(&resources, &issuer.base).await;
    assert!(
        resources
            .snapshot()
            .workspace_binding_incarnation(KEY)
            .is_some()
    );
    let service = AuthService::test_workspace_binding(resources, Arc::clone(&store));
    let list = service
        .list_auth_statuses(tonic::Request::new(ListAuthStatusesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let status = list
        .statuses
        .iter()
        .find(|status| status.target.as_ref() == Some(&proto_target(KEY)))
        .unwrap();
    assert_eq!(status.supported_methods, [AuthMethod::OauthDevice as i32]);
    assert_eq!(status.kind, AuthStatusKind::NotConfigured as i32);
    assert_eq!(issuer.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .begin_auth(begin_request(AuthMethod::OauthManual))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );

    let begin = service
        .begin_auth(begin_request(AuthMethod::OauthDevice))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(begin.verification_url, format!("{}/verify", issuer.base));
    assert_eq!(begin.user_code, "PAGE-CODE");
    assert_eq!(
        begin.completion_mode,
        AuthCompletionMode::ServerDevicePoll as i32
    );
    assert!(begin.auth_url.is_empty());
    assert!(!format!("{begin:?}").contains(DEVICE_SECRET));
    assert_eq!(
        service
            .complete_auth(tonic::Request::new(CompleteAuthRequest {
                attempt_id: begin.attempt_id.clone(),
                input: "ignored".to_owned()
            }))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    let completed = complete_device(&service, &begin.attempt_id).await;
    assert_eq!(
        completed.attempt.unwrap().lifecycle,
        AuthAttemptLifecycle::Succeeded as i32
    );
    assert_eq!(
        completed.status.unwrap().kind,
        AuthStatusKind::OauthConfigured as i32
    );

    let endpoint = Url::parse(&issuer.base).unwrap();
    let unrelated = endpoint.join("other").unwrap();
    assert!(
        service
            .workspace_binding_credential_lease(KEY, &unrelated)
            .await
            .unwrap()
            .is_none()
    );
    let initial = service
        .workspace_binding_credential_lease(KEY, &endpoint)
        .await
        .unwrap()
        .unwrap();
    assert!(initial.expires_at_ms <= now_ms() + 120_000);
    assert!(!format!("{initial:?}").contains(&initial.access_token));
    assert_eq!(
        service
            .begin_auth(begin_request(AuthMethod::OauthDevice))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );

    // A fresh catalog and auth service restore the SQL-backed completed grant.
    drop(service);
    let service = AuthService::test_workspace_binding(cold_catalog(&issuer), store);
    let restored = service
        .workspace_binding_credential_lease(KEY, &endpoint)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial.authority_id, restored.authority_id);
    assert_eq!(initial.access_token, restored.access_token);

    let refresh = || {
        tonic::Request::new(RefreshAuthRequest {
            target: Some(proto_target(KEY)),
        })
    };
    service.refresh_auth(refresh()).await.unwrap();
    let rotated = service
        .workspace_binding_credential_lease(KEY, &endpoint)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial.authority_id, rotated.authority_id);
    assert_ne!(initial.access_token, rotated.access_token);
    assert!(rotated.expires_at_ms <= now_ms() + 600_000);
    issuer.other_account.store(true, Ordering::SeqCst);
    assert_eq!(
        service.refresh_auth(refresh()).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    assert!(
        service
            .workspace_binding_credential_lease(KEY, &endpoint)
            .await
            .unwrap()
            .is_none()
    );

    let status = service
        .logout(tonic::Request::new(LogoutRequest {
            target: Some(proto_target(KEY)),
        }))
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(status.kind, AuthStatusKind::NotConfigured as i32);
    assert!(
        service
            .workspace_binding_credential_lease(KEY, &endpoint)
            .await
            .unwrap()
            .is_none()
    );
    server.abort();
}

#[tokio::test]
async fn workspace_binding_device_grant_does_not_survive_live_endpoint_aba() {
    let (issuer, server) = issuer().await;
    let (resources, store) = setup(&issuer).await;
    let service = AuthService::test_workspace_binding(resources.clone(), store);
    let endpoint = Url::parse(&issuer.base).unwrap();
    let begin = service
        .begin_auth(begin_request(AuthMethod::OauthDevice))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        complete_device(&service, &begin.attempt_id)
            .await
            .attempt
            .unwrap()
            .lifecycle,
        AuthAttemptLifecycle::Succeeded as i32
    );
    let lease = service
        .workspace_binding_credential_lease(KEY, &endpoint)
        .await
        .unwrap()
        .unwrap();

    replace_slot(
        &resources,
        "workspace-bindings/unrelated.yaml",
        binding_yaml("https://unrelated.example/").replace("name: export", "name: unrelated"),
    )
    .await;
    assert_eq!(
        service
            .workspace_binding_credential_lease(KEY, &endpoint)
            .await
            .unwrap()
            .unwrap()
            .authority_id,
        lease.authority_id
    );

    // No auth call observes the intermediate publication.
    let initial = resources.snapshot();
    replace_binding(&resources, "https://other.example/").await;
    replace_binding(&resources, &issuer.base).await;
    assert_eq!(
        resources.snapshot().workspace_bindings()[KEY],
        initial.workspace_bindings()[KEY]
    );
    assert!(
        service
            .workspace_binding_credential_lease(KEY, &endpoint)
            .await
            .unwrap()
            .is_none()
    );
    let attempt = complete_device(&service, &begin.attempt_id).await;
    assert_eq!(
        attempt.attempt.unwrap().lifecycle,
        AuthAttemptLifecycle::Canceled as i32
    );
    assert!(attempt.status.is_none());
    let list = service
        .list_auth_statuses(tonic::Request::new(ListAuthStatusesRequest {}))
        .await
        .unwrap()
        .into_inner();
    let status = list
        .statuses
        .iter()
        .find(|status| status.target.as_ref() == Some(&proto_target(KEY)))
        .unwrap();
    assert_eq!(status.kind, AuthStatusKind::AuthorizationRequired as i32);
    server.abort();
}

#[tokio::test]
async fn workspace_binding_cancel_unsupported_and_mismatched_discovery_fail_closed() {
    let (issuer, server) = issuer().await;
    let service = service(&issuer).await;
    issuer.advertise_device.store(false, Ordering::SeqCst);
    assert_eq!(
        service
            .begin_auth(begin_request(AuthMethod::OauthDevice))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    issuer.advertise_device.store(true, Ordering::SeqCst);
    issuer.wrong_resource.store(true, Ordering::SeqCst);
    assert_eq!(
        service
            .begin_auth(begin_request(AuthMethod::OauthDevice))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(issuer.starts.load(Ordering::SeqCst), 0);
    issuer.wrong_resource.store(false, Ordering::SeqCst);
    issuer.leak_device_code.store(true, Ordering::SeqCst);
    let leaked = service
        .begin_auth(begin_request(AuthMethod::OauthDevice))
        .await
        .unwrap_err();
    assert_eq!(leaked.code(), tonic::Code::DataLoss);
    assert!(!format!("{leaked:?}").contains(DEVICE_SECRET));
    issuer.leak_device_code.store(false, Ordering::SeqCst);

    for reset in [false, true] {
        let begin = service
            .begin_auth(begin_request(AuthMethod::OauthDevice))
            .await
            .unwrap()
            .into_inner();
        if reset {
            service
                .logout(tonic::Request::new(LogoutRequest {
                    target: Some(proto_target(KEY)),
                }))
                .await
                .unwrap();
        } else {
            service
                .cancel_auth(tonic::Request::new(CancelAuthRequest {
                    attempt_id: begin.attempt_id.clone(),
                }))
                .await
                .unwrap();
        }
        let attempt = service
            .get_auth_attempt_status(tonic::Request::new(GetAuthAttemptStatusRequest {
                attempt_id: begin.attempt_id,
            }))
            .await
            .unwrap()
            .into_inner()
            .attempt
            .unwrap();
        assert_eq!(attempt.lifecycle, AuthAttemptLifecycle::Canceled as i32);
    }
    assert_eq!(issuer.polls.load(Ordering::SeqCst), 0);
    server.abort();
}
