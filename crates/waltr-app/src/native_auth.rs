//! Generation-local access coordinator and verified credential-store binding.
use crate::credential_store::CredentialIdentity;
use waltr_client::{
    NativeAccessConfig, NativeAccessCoordinator, NativeCredentialProvider, NativeRefreshTokenStore,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct AccessBinding {
    pub config: NativeAccessConfig,
    pub store: Arc<dyn NativeRefreshTokenStore>,
    pub server_id: String,
    pub issuer: String,
}
pub(crate) fn browser_url(input: &str) -> Result<url::Url, String> {
    if input.len() > 16_384 || input.chars().any(char::is_control) {
        return Err("Invalid authorization URL".into());
    }
    crate::server_profiles::normalize_endpoint(input.split('?').next().unwrap_or_default())
        .map_err(|_| "Invalid authorization URL")?;
    let url = url::Url::parse(input).map_err(|_| "Invalid authorization URL")?;
    if url.fragment().is_some() {
        return Err("Invalid authorization URL".into());
    }
    Ok(url)
}
impl AccessBinding {
    pub fn from_bootstrap(
        server_id: String,
        bootstrap: waltr_client::NativeAccessBootstrap,
    ) -> Result<Self, String> {
        let config = bootstrap.config;
        let store = crate::credential_store::CredentialStore::new(CredentialIdentity {
            server_id: &server_id,
            endpoint: &config.endpoint,
            issuer: &bootstrap.issuer,
            client_id: &config.client_id,
            scopes: &config.scopes,
            audience: config.audience.as_deref(),
        })
        .map_err(|_| "Secure credentials are unavailable")?;
        Ok(Self {
            config,
            store,
            server_id,
            issuer: bootstrap.issuer,
        })
    }
    pub fn fresh(&self) -> Result<Self, String> {
        Self::from_bootstrap(
            self.server_id.clone(),
            waltr_client::NativeAccessBootstrap {
                config: self.config.clone(),
                issuer: self.issuer.clone(),
            },
        )
    }
    pub fn spawn(&self, credentials: NativeCredentialProvider) -> NativeAccessCoordinator {
        NativeAccessCoordinator::spawn_with_store(
            self.config.clone(),
            credentials,
            Arc::clone(&self.store),
            8,
        )
    }
}

#[derive(Default)]
pub(crate) struct AccessHost {
    checked_lease: Option<waltr_client::NativeCredentialLease>,
}

impl AccessHost {
    pub fn starts_probe(
        snapshot: &waltr_client::ClientSnapshot,
        command: &waltr_client::ClientCommand,
    ) -> bool {
        match command {
            waltr_client::ClientCommand::ReprobeWaltrAccess => {
                snapshot.access.status == waltr_client::AccessStatus::LoginRequired
            }
            waltr_client::ClientCommand::CompleteWaltrLogin { attempt_id } => {
                snapshot.access.active_attempt_id.as_ref() == Some(attempt_id)
            }
            _ => false,
        }
    }

    pub async fn cancel_command(
        &mut self,
        access: Option<&NativeAccessCoordinator>,
        credentials: &NativeCredentialProvider,
        snapshot: &waltr_client::ClientSnapshot,
        command: &waltr_client::ClientCommand,
    ) {
        if let Some(access) = access
            && let waltr_client::ClientCommand::CancelWaltrLogin { attempt_id } = command
            && snapshot.access.status == waltr_client::AccessStatus::LoginInProgress
            && snapshot.access.active_attempt_id.as_ref() == Some(attempt_id)
        {
            let _ = credentials.clear_bearer_token();
            self.checked_lease = None;
            drop(
                access
                    .action_sender()
                    .send(waltr_client::NativeAccessAction::Cancel)
                    .await,
            );
        }
    }

    pub async fn observe(
        &mut self,
        access: Option<&NativeAccessCoordinator>,
        credentials: &NativeCredentialProvider,
        snapshot: &waltr_client::ClientSnapshot,
        started_probe: bool,
    ) {
        let Some(access) = access else {
            return;
        };
        if snapshot.access.status == waltr_client::AccessStatus::LoginInProgress
            && let Some(attempt_id) = snapshot.access.active_attempt_id.clone()
        {
            drop(
                access
                    .action_sender()
                    .send(waltr_client::NativeAccessAction::AttemptStarted { attempt_id })
                    .await,
            );
        }
        let lease = credentials.credential_lease();
        if snapshot.access.status == waltr_client::AccessStatus::LoginRequired
            && self.checked_lease == Some(lease)
            && credentials.has_bearer_token()
        {
            let _ = credentials.clear_bearer_token();
            self.checked_lease = None;
            drop(
                access
                    .action_sender()
                    .send(waltr_client::NativeAccessAction::CredentialRejected)
                    .await,
            );
        } else if (started_probe
            && snapshot.access.status == waltr_client::AccessStatus::UnknownChecking)
            || snapshot.access.status == waltr_client::AccessStatus::Authenticated
        {
            // Unrelated checking snapshots cannot reclassify an in-flight anonymous probe.
            self.checked_lease = credentials.has_bearer_token().then_some(lease);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr_client::{
        AccessStatus, ClientCommand, ClientRuntime, ClientSnapshot, NativeAccessAction,
        NativeAccessOutput, NativeAccessProgress, NativeAuthorizationCallback,
        NativeCallbackPolicy, NativeRefreshTokenStoreError,
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        sync::{Notify, mpsc},
    };

    #[derive(Debug, Default)]
    struct TestStore {
        token: Mutex<Option<Vec<u8>>>,
        writes: AtomicUsize,
        deleted: Notify,
    }
    #[async_trait::async_trait]
    impl NativeRefreshTokenStore for TestStore {
        async fn load(&self) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
            Ok(None)
        }
        async fn store(&self, token: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
            *self.token.lock().unwrap() = Some(token.to_vec());
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn delete(&self) -> Result<(), NativeRefreshTokenStoreError> {
            *self.token.lock().unwrap() = None;
            self.deleted.notify_one();
            Ok(())
        }
    }
    fn config(endpoint: String) -> NativeAccessConfig {
        NativeAccessConfig {
            endpoint,
            client_id: "native".into(),
            scopes: vec!["waltr:api".into()],
            audience: None,
            callback: NativeCallbackPolicy::External,
        }
    }
    async fn output(coordinator: &mut NativeAccessCoordinator) -> NativeAccessOutput {
        tokio::time::timeout(std::time::Duration::from_secs(3), coordinator.next_output())
            .await
            .unwrap()
            .unwrap()
    }
    async fn issuer() -> (String, mpsc::Receiver<()>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let base = endpoint.clone();
        let (tokens, received) = mpsc::channel(8);
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < 16_384);
                    if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let body = match request.split_whitespace().nth(1).unwrap() {
                    "/.well-known/oauth-protected-resource" => format!(
                        r#"{{"resource":"{base}","authorization_servers":["{base}"],"scopes_supported":["waltr:api"]}}"#
                    ),
                    "/.well-known/openid-configuration" => format!(
                        r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","response_types_supported":["code"],"grant_types_supported":["authorization_code"],"code_challenge_methods_supported":["S256"]}}"#
                    ),
                    "/token" => {
                        tokens.send(()).await.unwrap();
                        r#"{"access_token":"test-access","refresh_token":"test-refresh","expires_in":3600,"token_type":"Bearer"}"#.into()
                    }
                    path => panic!("unexpected test path {path}"),
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        (endpoint, received, server)
    }
    async fn begin(
        host: &mut AccessHost,
        coordinator: &mut NativeAccessCoordinator,
        credentials: &NativeCredentialProvider,
        attempt: &str,
    ) -> (ClientSnapshot, String) {
        coordinator
            .action_sender()
            .send(NativeAccessAction::Start)
            .await
            .unwrap();
        assert!(
            matches!(output(coordinator).await, NativeAccessOutput::ClientCommand { command, .. } if *command == ClientCommand::StartWaltrLogin)
        );
        let mut snapshot = ClientRuntime::for_endpoint("http://127.0.0.1:1").snapshot();
        snapshot.access.status = AccessStatus::LoginInProgress;
        snapshot.access.active_attempt_id = Some(attempt.into());
        host.observe(Some(coordinator), credentials, &snapshot, false)
            .await;
        loop {
            if let NativeAccessOutput::Progress(NativeAccessProgress::AwaitingBrowser {
                authorization_url,
                ..
            }) = output(coordinator).await
            {
                let state = url::Url::parse(&authorization_url)
                    .unwrap()
                    .query_pairs()
                    .find(|(key, _)| key == "state")
                    .unwrap()
                    .1
                    .into_owned();
                return (snapshot, state);
            }
        }
    }
    async fn canceled(coordinator: &mut NativeAccessCoordinator, attempt: &str) {
        assert!(matches!(
            output(coordinator).await,
            NativeAccessOutput::Progress(NativeAccessProgress::Canceled)
        ));
        assert!(
            matches!(output(coordinator).await, NativeAccessOutput::ClientCommand { command, .. } if *command == ClientCommand::CancelWaltrLogin { attempt_id: attempt.into() })
        );
    }

    #[tokio::test]
    async fn host_cancel_fences_lease_and_discards_deferred_callback() {
        let (endpoint, mut tokens, server) = issuer().await;
        let credentials = NativeCredentialProvider::new();
        let store = Arc::new(TestStore::default());
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            config(endpoint),
            credentials.clone(),
            store.clone(),
            8,
        );
        let mut host = AccessHost::default();
        let (snapshot, state) = begin(&mut host, &mut coordinator, &credentials, "first").await;
        let lease = credentials.credential_lease();
        host.cancel_command(
            Some(&coordinator),
            &credentials,
            &snapshot,
            &ClientCommand::CancelWaltrLogin {
                attempt_id: "stale".into(),
            },
        )
        .await;
        assert_eq!(credentials.credential_lease(), lease);
        host.cancel_command(
            Some(&coordinator),
            &credentials,
            &snapshot,
            &ClientCommand::CancelWaltrLogin {
                attempt_id: "first".into(),
            },
        )
        .await;
        assert_ne!(credentials.credential_lease(), lease);
        coordinator
            .action_sender()
            .send(NativeAccessAction::AuthorizationCallback(
                NativeAuthorizationCallback::new(format!(
                    "dev.waltr.app:/oauth2redirect?state={state}&code=late"
                )),
            ))
            .await
            .unwrap();
        canceled(&mut coordinator, "first").await;
        // A subsequent Start output is a barrier after the queued late callback.
        coordinator
            .action_sender()
            .send(NativeAccessAction::Start)
            .await
            .unwrap();
        assert!(
            matches!(output(&mut coordinator).await, NativeAccessOutput::ClientCommand { command, .. } if *command == ClientCommand::StartWaltrLogin)
        );
        assert!(tokens.try_recv().is_err());
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        assert!(!credentials.has_bearer_token());
        coordinator.stop().await;
        server.abort();
    }

    #[tokio::test]
    async fn host_cancel_allows_retry_and_only_new_callback_can_persist() {
        let (endpoint, mut tokens, server) = issuer().await;
        let credentials = NativeCredentialProvider::new();
        let store = Arc::new(TestStore::default());
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            config(endpoint),
            credentials.clone(),
            store.clone(),
            8,
        );
        let mut host = AccessHost::default();
        let (snapshot, old_state) = begin(&mut host, &mut coordinator, &credentials, "first").await;
        host.cancel_command(
            Some(&coordinator),
            &credentials,
            &snapshot,
            &ClientCommand::CancelWaltrLogin {
                attempt_id: "first".into(),
            },
        )
        .await;
        canceled(&mut coordinator, "first").await;
        let (_, state) = begin(&mut host, &mut coordinator, &credentials, "second").await;
        assert_ne!(old_state, state);
        for state in [old_state, state] {
            coordinator
                .action_sender()
                .send(NativeAccessAction::AuthorizationCallback(
                    NativeAuthorizationCallback::new(format!(
                        "dev.waltr.app:/oauth2redirect?state={state}&code=accepted"
                    )),
                ))
                .await
                .unwrap();
        }
        loop {
            if let NativeAccessOutput::ClientCommand { command, .. } =
                output(&mut coordinator).await
            {
                assert_eq!(
                    *command,
                    ClientCommand::CompleteWaltrLogin {
                        attempt_id: "second".into()
                    }
                );
                break;
            }
        }
        assert_eq!(tokens.recv().await, Some(()));
        assert!(tokens.try_recv().is_err());
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        assert!(credentials.has_bearer_token());
        coordinator.stop().await;
        server.abort();
    }

    #[tokio::test]
    async fn host_rejects_only_a_bearer_observed_on_protected_check() {
        let credentials = NativeCredentialProvider::new();
        let store = Arc::new(TestStore::default());
        let coordinator = NativeAccessCoordinator::spawn_with_store(
            config("http://127.0.0.1:1".into()),
            credentials.clone(),
            store.clone(),
            8,
        );
        let mut host = AccessHost::default();
        let mut snapshot = ClientRuntime::for_endpoint("http://127.0.0.1:1").snapshot();
        snapshot.access.status = AccessStatus::UnknownChecking;
        host.observe(Some(&coordinator), &credentials, &snapshot, false)
            .await;
        credentials
            .replace_bearer_token("installed-after-anonymous-probe")
            .unwrap();
        host.observe(Some(&coordinator), &credentials, &snapshot, false)
            .await;
        snapshot.access.status = AccessStatus::LoginRequired;
        host.observe(Some(&coordinator), &credentials, &snapshot, false)
            .await;
        assert!(credentials.has_bearer_token());
        let started_probe = AccessHost::starts_probe(&snapshot, &ClientCommand::ReprobeWaltrAccess);
        assert!(started_probe);
        assert!(!AccessHost::starts_probe(
            &snapshot,
            &ClientCommand::CompleteWaltrLogin {
                attempt_id: "stale".into()
            }
        ));
        *store.token.lock().unwrap() = Some(b"durable-refresh".to_vec());
        snapshot.access.status = AccessStatus::UnknownChecking;
        host.observe(Some(&coordinator), &credentials, &snapshot, started_probe)
            .await;
        host.observe(Some(&coordinator), &credentials, &snapshot, false)
            .await;
        assert!(
            credentials.has_bearer_token(),
            "transient probe failure must not delete credentials"
        );
        assert!(store.token.lock().unwrap().is_some());
        snapshot.access.status = AccessStatus::LoginRequired;
        host.observe(Some(&coordinator), &credentials, &snapshot, false)
            .await;
        assert!(!credentials.has_bearer_token());
        tokio::time::timeout(std::time::Duration::from_secs(3), store.deleted.notified())
            .await
            .unwrap();
        assert!(store.token.lock().unwrap().is_none());
        credentials
            .replace_bearer_token("replacement-bearer")
            .unwrap();
        *store.token.lock().unwrap() = Some(b"replacement-refresh".to_vec());
        snapshot.access.status = AccessStatus::Authenticated;
        host.observe(Some(&coordinator), &credentials, &snapshot, false)
            .await;
        snapshot.access.status = AccessStatus::LoginRequired;
        host.observe(Some(&coordinator), &credentials, &snapshot, false)
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(3), store.deleted.notified())
            .await
            .unwrap();
        assert!(!credentials.has_bearer_token());
        assert!(store.token.lock().unwrap().is_none());
        coordinator.stop().await;
    }

    #[test]
    fn authorization_browser_accepts_only_validated_network_origins() {
        for allowed in [
            "https://issuer.example/authorize?state=private",
            "http://127.0.0.1:8910/authorize?state=private",
            "http://[::1]:8910/authorize",
        ] {
            assert!(browser_url(allowed).is_ok(), "{allowed}");
        }
        for forbidden in [
            "javascript:alert(1)",
            "https://user:secret@issuer.example/auth",
            "https://@issuer.example/auth",
            "http://issuer.example/auth",
            "http://127.1/auth",
            "http://2130706433/auth",
            "https://issuer.example/auth#private",
            "https://issuer.example/auth?state=private\n",
        ] {
            assert_eq!(
                browser_url(forbidden).unwrap_err(),
                "Invalid authorization URL"
            );
        }
    }
}
