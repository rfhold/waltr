use crate::{RoutineInteractionTransport, interaction::InteractionLimits};
use futures_util::{
    StreamExt,
    future::{AbortHandle, Abortable},
};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::prelude::*;

#[derive(Default)]
pub(super) struct Host {
    pub disposed: bool,
    next: u32,
    active: Option<(u32, AbortHandle)>,
    status: Option<Status>,
}
impl Host {
    pub fn cancel(&mut self) {
        if let Some((_, abort)) = self.active.take() {
            abort.abort();
        }
        self.status = None;
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.cancel();
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    routine: String,
    trigger: String,
    input: String,
    output: String,
    session_id: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    operation_id: u32,
    phase: &'static str,
    preview: String,
    final_text: Option<String>,
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./".contains(&b))
}

use super::WaltrClient;

#[wasm_bindgen]
impl WaltrClient {
    /// Binary PCM uses its own bounded lane, never JSON or reducer commands.
    // Preserve the owned-string wasm-bindgen boundary used by the client API.
    #[allow(clippy::needless_pass_by_value)]
    #[wasm_bindgen(js_name = startAudioRun)]
    pub fn start_audio_run(&self, metadata_json: String, pcm: &[u8]) -> Result<u32, JsValue> {
        if metadata_json.len() > 4096
            || pcm.len() > 32 * 1024 * 1024 - 65536
            || pcm.is_empty()
            || !pcm.len().is_multiple_of(2)
        {
            return Err(JsValue::from_str("invalid bounded audio input"));
        }
        let meta: Metadata = serde_json::from_str(&metadata_json).map_err(super::js_error)?;
        if ![&meta.routine, &meta.trigger, &meta.input, &meta.output]
            .into_iter()
            .all(|s| identifier(s))
            || meta.session_id.is_empty()
            || meta.session_id.len() > 256
        {
            return Err(JsValue::from_str("invalid audio selection"));
        }
        let mut host = self.interactions.borrow_mut();
        if host.disposed || host.active.is_some() {
            return Err(JsValue::from_str("audio host unavailable or busy"));
        }
        // Session identity is a local insertion fence, not a Routine source session.
        let request = crate::interaction::browser_audio_run_request(
            meta.routine,
            meta.trigger,
            meta.input,
            pcm,
        );
        let limits = InteractionLimits::default();
        crate::interaction::validate_complete_interaction(&request, &limits)
            .map_err(super::js_error)?;
        host.next = host
            .next
            .checked_add(1)
            .ok_or_else(|| JsValue::from_str("audio operation limit"))?;
        let id = host.next;
        let (abort, registration) = AbortHandle::new_pair();
        host.active = Some((id, abort));
        host.status = Some(Status {
            operation_id: id,
            phase: "processing",
            preview: String::new(),
            final_text: None,
        });
        drop(host);
        let weak = Rc::downgrade(&self.interactions);
        let transport = self.interaction_transport.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let drive = async {
                let result = transport.run_interaction(request, limits).await;
                let Ok(mut output) = result else {
                    update(&weak, id, "error", String::new(), None);
                    return;
                };
                while let Some(event) = output.next().await {
                    if event.is_err() {
                        update(&weak, id, "error", String::new(), None);
                        return;
                    }
                    update(
                        &weak,
                        id,
                        "processing",
                        output.preview_text(&meta.output).unwrap_or_default(),
                        None,
                    );
                }
                match output.final_text(&meta.output) {
                    Ok(text) => update(&weak, id, "succeeded", text.clone(), Some(text)),
                    Err(_) => update(&weak, id, "error", String::new(), None),
                }
            };
            let _ = Abortable::new(drive, registration).await;
        });
        Ok(id)
    }
    #[wasm_bindgen(js_name = audioRunStatusJson)]
    pub fn audio_run_status_json(&self, operation_id: u32) -> Result<String, JsValue> {
        let host = self.interactions.borrow();
        let status = host
            .status
            .as_ref()
            .filter(|s| s.operation_id == operation_id)
            .ok_or_else(|| JsValue::from_str("stale audio operation"))?;
        serde_json::to_string(status).map_err(super::js_error)
    }
    #[wasm_bindgen(js_name = cancelAudioRun)]
    pub fn cancel_audio_run(&self, operation_id: u32) {
        let mut host = self.interactions.borrow_mut();
        if host
            .status
            .as_ref()
            .is_some_and(|s| s.operation_id == operation_id)
        {
            host.cancel();
        }
    }
}
fn update(
    host: &std::rc::Weak<RefCell<Host>>,
    id: u32,
    phase: &'static str,
    preview: String,
    final_text: Option<String>,
) {
    if let Some(host) = host.upgrade() {
        let mut host = host.borrow_mut();
        if host.status.as_ref().is_some_and(|s| s.operation_id == id) {
            host.status = Some(Status {
                operation_id: id,
                phase,
                preview,
                final_text,
            });
            if phase != "processing" {
                host.active = None;
            }
        }
    }
}
