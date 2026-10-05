//! Process-owned macOS Apps assets, with an optional disposable attack probe.

use std::io;

#[cfg(feature = "native-apps-test")]
use axum::{Router, routing::get};
use waltr_server::{
    app_container::{AppsContainerConfig, SpawnedAppsContainer, spawn_app_container},
    embedded::EmbeddedServer,
};
use serde::Serialize;
use tokio::sync::oneshot;
#[cfg(feature = "native-apps-test")]
use tokio::task::JoinHandle;

#[cfg(feature = "native-apps-test")]
pub(crate) const IDENTIFIER: &str = "dev.waltr.app.native-apps-test";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Bootstrap {
    pub(crate) container_url: String,
    #[cfg(feature = "native-apps-test")]
    pub(crate) probe_url: String,
}

impl std::fmt::Debug for Bootstrap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Bootstrap(<app-assets>)")
    }
}

#[cfg(feature = "native-apps-test")]
pub(crate) fn validate_profile(config: &tauri::Config) -> io::Result<()> {
    if config.identifier != IDENTIFIER {
        return Err(io::Error::other(
            "native-apps-test requires the disposable test configuration",
        ));
    }
    Ok(())
}

pub(crate) struct NativeAssets {
    container: SpawnedAppsContainer,
    bootstrap: Bootstrap,
    #[cfg(feature = "native-apps-test")]
    probe_stop: Option<oneshot::Sender<()>>,
    #[cfg(feature = "native-apps-test")]
    probe_task: JoinHandle<io::Result<()>>,
}

impl NativeAssets {
    pub(crate) async fn start() -> io::Result<Self> {
        let container = spawn_app_container(AppsContainerConfig {
            allowed_host_origins: vec!["tauri://localhost".to_owned()],
            ..Default::default()
        })
        .await
        .map_err(|_| io::Error::other("Apps listener failed"))?;
        #[cfg(feature = "native-apps-test")]
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) => {
                drop(container.shutdown().await);
                return Err(error);
            }
        };
        #[cfg(feature = "native-apps-test")]
        let probe_url = match listener.local_addr() {
            Ok(address) => format!("http://{address}/probe/container.html"),
            Err(error) => {
                drop(container.shutdown().await);
                return Err(error);
            }
        };
        let bootstrap = Bootstrap {
            container_url: format!("{}/container.html", container.public_origin),
            #[cfg(feature = "native-apps-test")]
            probe_url,
        };
        #[cfg(feature = "native-apps-test")]
        let router = Router::new()
            .route("/probe/container.html", get(|| async {
                ([ ("content-type", "text/html; charset=utf-8"),
                   ("content-security-policy", "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'unsafe-inline'; frame-src 'self'; connect-src ipc: http://ipc.localhost; frame-ancestors tauri://localhost"),
                   ("cache-control", "no-store") ], include_str!("../tests/native-apps/container.html"))
            }))
            .route("/probe/probe.js", get(|| async {
                ([("content-type", "text/javascript; charset=utf-8"), ("cache-control", "no-store")], include_str!("../tests/native-apps/probe.js"))
            }));
        #[cfg(feature = "native-apps-test")]
        let (probe_stop, stop) = oneshot::channel();
        #[cfg(feature = "native-apps-test")]
        let probe_task = tokio::spawn(serve_probe(listener, router, stop));
        Ok(Self {
            container,
            bootstrap,
            #[cfg(feature = "native-apps-test")]
            probe_stop: Some(probe_stop),
            #[cfg(feature = "native-apps-test")]
            probe_task,
        })
    }

    pub(crate) fn bootstrap(&self) -> Bootstrap {
        self.bootstrap.clone()
    }

    pub(crate) async fn shutdown(self) -> io::Result<()> {
        #[cfg(feature = "native-apps-test")]
        let mut self_ = self;
        #[cfg(not(feature = "native-apps-test"))]
        let self_ = self;
        #[cfg(feature = "native-apps-test")]
        let probe = {
            self_.probe_stop.take();
            (&mut self_.probe_task)
                .await
                .map_err(io::Error::other)
                .and_then(|result| result)
        };
        #[cfg(not(feature = "native-apps-test"))]
        let probe = Ok(());
        let container = self_.container.shutdown().await;
        probe.and(container)
    }

    pub(crate) async fn serve_with_parent(
        self,
        server: EmbeddedServer,
        shutdown: oneshot::Receiver<()>,
    ) -> io::Result<()> {
        #[cfg(feature = "native-apps-test")]
        let mut self_ = self;
        #[cfg(not(feature = "native-apps-test"))]
        let self_ = self;
        let (parent_stop, stop) = oneshot::channel::<()>();
        let parent = server.serve_with_shutdown(async {
            let _ = stop.await;
        });
        tokio::pin!(parent);
        tokio::pin!(shutdown);
        let container_stopped = self_.container.stopped();
        let probe_wait = async {
            #[cfg(feature = "native-apps-test")]
            {
                drop((&mut self_.probe_task).await);
            }
            #[cfg(not(feature = "native-apps-test"))]
            std::future::pending::<()>().await;
        };
        enum Exit {
            Requested,
            Parent(io::Result<()>),
            Probe,
            Container,
        }
        let exit = tokio::select! {
            _ = &mut shutdown => Exit::Requested,
            result = &mut parent => Exit::Parent(result),
            () = probe_wait => Exit::Probe,
            () = container_stopped.cancelled() => Exit::Container,
        };
        let requested = matches!(exit, Exit::Requested);
        #[cfg(feature = "native-apps-test")]
        let probe_exited = matches!(exit, Exit::Probe);
        drop(parent_stop);
        let parent_result = match exit {
            Exit::Parent(result) => result,
            _ => (&mut parent).await,
        };
        #[cfg(feature = "native-apps-test")]
        self_.probe_stop.take();
        #[cfg(feature = "native-apps-test")]
        let probe_result = if probe_exited {
            Ok(())
        } else {
            (&mut self_.probe_task)
                .await
                .map_err(io::Error::other)
                .and_then(|result| result)
        };
        #[cfg(not(feature = "native-apps-test"))]
        let probe_result = Ok(());
        let container_result = self_.container.shutdown().await;
        if !requested {
            return Err(io::Error::other("native Apps owner stopped unexpectedly"));
        }
        parent_result.and(probe_result).and(container_result)
    }
}

#[cfg(feature = "native-apps-test")]
async fn serve_probe(
    listener: tokio::net::TcpListener,
    router: Router,
    stop: oneshot::Receiver<()>,
) -> io::Result<()> {
    use hyper_util::{rt::TokioIo, service::TowerToHyperService};
    let mut connections = tokio::task::JoinSet::new();
    tokio::pin!(stop);
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut stop => break Ok(()),
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result { break Err(io::Error::other(error)); }
            },
            accepted = listener.accept() => {
                let (stream, _) = match accepted { Ok(accepted) => accepted, Err(error) => break Err(error) };
                let router = router.clone();
                connections.spawn(async move {
                    drop(hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), TowerToHyperService::new(router)).await);
                });
            },
        }
    };
    drop(listener);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "native-apps-test")]
    #[test]
    fn default_identifier_cannot_boot_test_feature() {
        let config: tauri::Config =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert!(validate_profile(&config).is_err());
    }

    #[tokio::test]
    async fn assets_are_separate_from_api_and_stop_with_owner() {
        let assets = NativeAssets::start().await.unwrap();
        #[cfg(not(feature = "native-apps-test"))]
        assert_eq!(
            serde_json::to_value(assets.bootstrap()).unwrap(),
            serde_json::json!({ "containerUrl": assets.bootstrap.container_url })
        );
        let url = assets.bootstrap.container_url.clone();
        #[cfg(feature = "native-apps-test")]
        let probe_url = assets.bootstrap.probe_url.clone();
        let stopped = assets.container.stopped();
        assert!(!stopped.is_cancelled());
        let client = reqwest::Client::new();
        assert!(client.get(&url).send().await.unwrap().status().is_success());
        let api = url.replace(
            "/container.html",
            "/waltr.session.v1.SessionService/ListSessions",
        );
        assert_eq!(client.get(api).send().await.unwrap().status(), 404);
        assert_eq!(
            client
                .get(url.replace("/container.html", "/probe/container.html"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assets.shutdown().await.unwrap();
        assert!(stopped.is_cancelled());
        assert!(client.get(url).send().await.is_err());
        #[cfg(feature = "native-apps-test")]
        assert!(client.get(probe_url).send().await.is_err());
    }

    #[tokio::test]
    async fn asset_termination_signal_is_owned_and_does_not_depend_on_http_polling() {
        let assets = NativeAssets::start().await.unwrap();
        let stopped = assets.container.stopped();
        stopped.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), stopped.cancelled())
            .await
            .unwrap();
        assets.shutdown().await.unwrap();
    }
}
