//! Shared HTTP connection ownership and bounded shutdown cleanup.

use axum::Router;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct OwnedHttpExecutor(Arc<Mutex<HttpTasks>>);

struct HttpTasks {
    closing: bool,
    tasks: tokio::task::JoinSet<()>,
}

impl OwnedHttpExecutor {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(HttpTasks {
            closing: false,
            tasks: tokio::task::JoinSet::new(),
        })))
    }

    fn reap(&self) {
        let mut state = self.0.lock().unwrap();
        while let Some(result) = state.tasks.try_join_next() {
            if let Err(error) = result {
                tracing::debug!(%error, "HTTP request task ended with error");
            }
        }
    }

    async fn shutdown(&self) {
        let mut tasks = {
            let mut state = self.0.lock().unwrap();
            state.closing = true;
            std::mem::replace(&mut state.tasks, tokio::task::JoinSet::new())
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

impl<F> hyper::rt::Executor<F> for OwnedHttpExecutor
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        let mut state = self.0.lock().unwrap();
        if state.closing {
            // Rejected futures can run recursive executor calls while being dropped.
            drop(state);
            drop(future);
            return;
        }
        state.tasks.spawn(async move {
            let _output = future.await;
        });
    }
}

/// Own both socket connections and Hyper-spawned HTTP/2 service futures until cleanup completes.
pub(crate) async fn serve_owned_connections<F>(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send,
{
    use hyper_util::{rt::TokioIo, server::conn::auto::Builder, service::TowerToHyperService};

    let mut connections = tokio::task::JoinSet::new();
    let executor = OwnedHttpExecutor::new();
    let mut reap = tokio::time::interval(Duration::from_millis(100));
    reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let drain = CancellationToken::new();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break Ok(()),
            _ = reap.tick() => executor.reap(),
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = joined {
                    break Err(std::io::Error::other(error));
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                let service = TowerToHyperService::new(router.clone().layer(axum::Extension(axum::extract::ConnectInfo(peer))));
                let drain = drain.clone();
                let executor = executor.clone();
                connections.spawn(async move {
                    let builder = Builder::new(executor);
                    let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    let result = tokio::select! {
                        result = &mut connection => result,
                        () = drain.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            connection.await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(%error, "HTTP connection closed with error");
                    }
                });
            }
        }
    };
    drop(listener);
    drain.cancel();
    let deadline = tokio::time::sleep(CONNECTION_DRAIN_TIMEOUT);
    tokio::pin!(deadline);
    while !connections.is_empty() {
        tokio::select! {
            _ = connections.join_next() => {},
            () = &mut deadline => {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                break;
            }
        }
    }
    executor.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, routing::get};

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn http2_pending_request_cleanup_finishes_before_owner_returns() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        struct Cleanup(Arc<AtomicBool>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(500));
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let cleaned = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(tokio::sync::Notify::new());
        let dispatched = Arc::new(AtomicUsize::new(0));
        let handler_cleaned = cleaned.clone();
        let handler_entered = entered.clone();
        let handler_dispatched = dispatched.clone();
        let router = Router::new().route(
            "/pending",
            get(move |request: axum::extract::Request| {
                let cleaned = handler_cleaned.clone();
                let entered = handler_entered.clone();
                let dispatched = handler_dispatched.clone();
                async move {
                    assert_eq!(request.version(), axum::http::Version::HTTP_2);
                    let _guard = Cleanup(cleaned);
                    dispatched.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    std::future::pending::<StatusCode>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_owned_connections(
            listener,
            router,
            cancel.clone().cancelled_owned(),
        ));
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let request_client = client.clone();
        let request = tokio::spawn(async move {
            request_client
                .get(format!("http://{addr}/pending"))
                .send()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert!(!cleaned.load(Ordering::SeqCst));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(8), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            cleaned.load(Ordering::SeqCst),
            "HTTP/2 service cleanup outlived its owner"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(2), request)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            client
                .get(format!("http://{addr}/pending"))
                .send()
                .await
                .is_err()
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn executor_reaps_completed_tasks_and_rejects_recursive_shutdown_admission() {
        use hyper::rt::Executor as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct RecursiveDrop {
            executor: OwnedHttpExecutor,
            dispatched: Arc<AtomicUsize>,
        }
        impl Drop for RecursiveDrop {
            fn drop(&mut self) {
                let dispatched = self.dispatched.clone();
                self.executor.execute(async move {
                    dispatched.fetch_add(1, Ordering::SeqCst);
                });
            }
        }
        let executor = OwnedHttpExecutor::new();
        for _ in 0..100 {
            executor.execute(async {});
        }
        while !executor.0.lock().unwrap().tasks.is_empty() {
            tokio::task::yield_now().await;
            executor.reap();
        }
        let dispatched = Arc::new(AtomicUsize::new(0));
        let drop_guard = RecursiveDrop {
            executor: executor.clone(),
            dispatched: dispatched.clone(),
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let task_entered = entered.clone();
        executor.execute(async move {
            let _guard = drop_guard;
            task_entered.notify_one();
            std::future::pending::<()>().await;
        });
        entered.notified().await;
        executor.shutdown().await;
        tokio::task::yield_now().await;
        assert_eq!(dispatched.load(Ordering::SeqCst), 0);
        assert!(executor.0.lock().unwrap().closing);
        assert!(executor.0.lock().unwrap().tasks.is_empty());
    }
}
