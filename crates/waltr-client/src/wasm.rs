//! Browser WebAssembly bindings for the shared Waltr client.

use std::{cell::RefCell, rc::Rc};

use futures_channel::{mpsc, oneshot};
use futures_util::{
    StreamExt,
    future::{Either, select},
    lock::Mutex,
    pin_mut,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::{ClientCommand, ClientCommandRequest, ClientRuntime, ClientStateStreamEvent};

#[wasm_bindgen(
    inline_js = "export function sleepMs(ms) { return new Promise((resolve) => setTimeout(resolve, ms)); }"
)]
unsafe extern "C" {
    #[wasm_bindgen(js_name = sleepMs)]
    async fn sleep_ms(milliseconds: u32);
}

/// Browser-facing shared reducer client.
#[derive(Clone)]
#[wasm_bindgen]
pub struct WaltrClient {
    endpoint: String,
    commands: mpsc::UnboundedSender<ClientCommandRequest>,
    latest_snapshot: Rc<RefCell<String>>,
    snapshots: Rc<Mutex<mpsc::UnboundedReceiver<String>>>,
    stop: Rc<RefCell<Option<oneshot::Sender<()>>>>,
}

impl std::fmt::Debug for WaltrClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaltrClient")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

#[wasm_bindgen]
#[allow(tail_expr_drop_order)]
impl WaltrClient {
    /// Create a browser client for an optional API base URL.
    ///
    /// Blank or omitted values use the current browser origin, allowing Vite's
    /// development proxy to handle `/waltr...` requests without CORS.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new(endpoint: Option<String>) -> Self {
        let endpoint = normalize_endpoint(endpoint);
        let runtime =
            ClientRuntime::for_endpoint(endpoint.clone()).into_state_stream(browser_sleep);
        let latest_snapshot = Rc::new(RefCell::new(
            serde_json::to_string(&runtime.snapshot()).unwrap_or_else(|error| error.to_string()),
        ));
        let (commands, command_receiver) = mpsc::unbounded();
        let (snapshots, snapshot_receiver) = mpsc::unbounded();
        let (stop, stop_receiver) = oneshot::channel();

        spawn_client_loop(
            runtime,
            command_receiver,
            snapshots,
            Rc::clone(&latest_snapshot),
            stop_receiver,
        );

        Self {
            endpoint,
            commands,
            latest_snapshot,
            snapshots: Rc::new(Mutex::new(snapshot_receiver)),
            stop: Rc::new(RefCell::new(Some(stop))),
        }
    }

    /// Create a browser client that targets the current origin.
    #[wasm_bindgen(js_name = forCurrentOrigin)]
    #[must_use]
    pub fn for_current_origin() -> Self {
        Self::new(None)
    }

    /// Return the normalized endpoint base URL.
    #[wasm_bindgen(js_name = endpoint)]
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    /// Return a JSON snapshot of the shared reducer state.
    #[wasm_bindgen(js_name = snapshotJson)]
    pub async fn snapshot_json(&self) -> Result<String, JsValue> {
        Ok(self.latest_snapshot.borrow().clone())
    }

    /// Dispatch a JSON command through the shared reducer runtime.
    #[wasm_bindgen(js_name = dispatchJson)]
    pub async fn dispatch_json(&self, command_json: String) -> Result<String, JsValue> {
        let command: ClientCommand = serde_json::from_str(&command_json).map_err(js_error)?;
        let (result, receiver) = oneshot::channel();

        self.commands
            .unbounded_send(ClientCommandRequest::with_result(command, result))
            .map_err(js_error)?;

        let snapshot = receiver.await.map_err(js_error)?.map_err(js_error)?;

        serde_json::to_string(&snapshot).map_err(js_error)
    }

    /// Close the command source and stop the shared runtime.
    #[wasm_bindgen(js_name = dispose)]
    pub fn dispose(&self) {
        let latest = self.latest_snapshot.borrow().clone();
        if let Ok(mut snapshot) = serde_json::from_str::<serde_json::Value>(&latest) {
            snapshot["apps"] = serde_json::json!({ "current": {}, "requests": {} });
            if let Ok(json) = serde_json::to_string(&snapshot) {
                *self.latest_snapshot.borrow_mut() = json;
            }
        }
        if let Some(mut snapshots) = self.snapshots.try_lock() {
            while snapshots.try_recv().is_ok() {}
        }
        self.commands.close_channel();
        if let Some(stop) = self.stop.borrow_mut().take() {
            let _ = stop.send(());
        }
    }

    /// Advance the shared state stream and return the next JSON snapshot.
    #[wasm_bindgen(js_name = nextStateJson)]
    pub async fn next_state_json(&self) -> Result<String, JsValue> {
        self.snapshots
            .lock()
            .await
            .next()
            .await
            .ok_or_else(|| JsValue::from_str("client state stream closed"))
    }

    /// Advance the shared state stream, returning the current snapshot on timeout.
    #[wasm_bindgen(js_name = nextStateJsonWithTimeout)]
    pub async fn next_state_json_with_timeout(&self, timeout_ms: u32) -> Result<String, JsValue> {
        let next = self.next_state_json();
        let timeout = sleep_ms(timeout_ms);
        pin_mut!(next);
        pin_mut!(timeout);

        match select(next, timeout).await {
            Either::Left((snapshot, _)) => snapshot,
            Either::Right(((), pending_next)) => {
                drop(pending_next);
                Ok(self.latest_snapshot.borrow().clone())
            }
        }
    }
}

fn spawn_client_loop(
    mut runtime: crate::ClientStateStream,
    mut commands: mpsc::UnboundedReceiver<ClientCommandRequest>,
    snapshots: mpsc::UnboundedSender<String>,
    latest_snapshot: Rc<RefCell<String>>,
    stop: oneshot::Receiver<()>,
) {
    spawn_local(async move {
        let run = async move {
            loop {
                let event = runtime.next_or_dispatch(commands.next()).await;
                let ClientStateStreamEvent::Snapshot(snapshot) = event else {
                    return;
                };

                let Ok(snapshot_json) = serde_json::to_string(snapshot.as_ref()) else {
                    return;
                };

                latest_snapshot.replace(snapshot_json.clone());
                if snapshots.unbounded_send(snapshot_json).is_err() {
                    return;
                }
            }
        };
        pin_mut!(run);
        pin_mut!(stop);
        drop(select(stop, run).await);
    });
}

async fn browser_sleep(delay: u64) {
    sleep_ms(delay.try_into().unwrap_or(u32::MAX)).await;
}

fn normalize_endpoint(endpoint: Option<String>) -> String {
    endpoint
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_owned()
}

fn js_error(error: impl ToString) -> JsValue {
    JsValue::from_str(&error.to_string())
}

#[cfg(test)]
mod tests {
    use super::WaltrClient;

    #[test]
    fn defaults_to_same_origin_endpoint() {
        let client = WaltrClient::for_current_origin();

        assert_eq!(client.endpoint(), "");
    }

    #[test]
    fn trims_custom_endpoint() {
        let client = WaltrClient::new(Some(" http://127.0.0.1:8080/ ".to_owned()));

        assert_eq!(client.endpoint(), "http://127.0.0.1:8080");
    }
}
