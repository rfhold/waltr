use wasm_bindgen::prelude::*;

use crate::{
    ClientCommand, ClientCommandRequest, ClientError, MAX_MESSAGE_IMAGE_BYTES,
    MAX_MESSAGE_IMAGE_COUNT, MAX_MESSAGE_TOTAL_IMAGE_BYTES, MessageImageFormatView,
    MessageInputPart,
    transport::message_images::{MAX_MESSAGE_PARTS, MAX_MESSAGE_TEXT_BYTES},
};

use super::{WaltrClient, js_error};

#[wasm_bindgen(inline_js = "
export function waltrPartsLength(parts) {
    return Array.isArray(parts) && parts.length <= 32 ? parts.length : -1;
}
export function waltrPartField(parts, index, field) {
    const value = parts[index]?.[field];
    return typeof value === 'string' && value.length <= 16384 ? value : undefined;
}
export function waltrImageBytes(parts, index) {
    const bytes = parts[index]?.bytes;
    return bytes instanceof Uint8Array ? bytes : undefined;
}
export function waltrImageLength(bytes) {
    return bytes.byteLength;
}
export function waltrCopyImage(bytes, length) {
    if (bytes.byteLength !== length) throw new Error('Image bytes changed');
    const copy = new Uint8Array(length);
    copy.set(bytes);
    return copy;
}
")]
unsafe extern "C" {
    #[wasm_bindgen(js_name = waltrPartsLength, catch)]
    fn parts_length(parts: &JsValue) -> Result<i32, JsValue>;
    #[wasm_bindgen(js_name = waltrPartField, catch)]
    fn part_field(parts: &JsValue, index: u32, field: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_name = waltrImageBytes, catch)]
    fn image_bytes(parts: &JsValue, index: u32) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_name = waltrImageLength, catch)]
    fn image_length(bytes: &JsValue) -> Result<f64, JsValue>;
    #[wasm_bindgen(js_name = waltrCopyImage, catch)]
    fn copy_image(bytes: &JsValue, length: u32) -> Result<Vec<u8>, JsValue>;
}

#[wasm_bindgen]
#[allow(tail_expr_drop_order, reason = "wasm-bindgen async method glue")]
impl WaltrClient {
    /// Upload and admit ordered text and `Uint8Array` image parts on this runtime.
    #[wasm_bindgen(js_name = sendMessageWithImages)]
    pub async fn send_message_with_images(
        &self,
        session_id: String,
        parts: JsValue,
    ) -> Result<(), JsValue> {
        let parts = decode_parts(&parts)?;
        self.message_images
            .send(session_id, parts)
            .await
            .map_err(|error| self.binary_error(error))
    }

    /// Read one authorized timeline image as a `Uint8Array`.
    #[wasm_bindgen(js_name = readMessageImage)]
    pub async fn read_message_image(
        &self,
        session_id: String,
        item_id: String,
        part_index: u32,
        size_bytes: u32,
    ) -> Result<Vec<u8>, JsValue> {
        self.message_images
            .read_size(session_id, item_id, part_index, size_bytes)
            .await
            .map_err(|error| self.binary_error(error))
    }

    /// Cancel the current composer operation without canceling history reads.
    #[wasm_bindgen(js_name = cancelMessageImageSend)]
    pub fn cancel_message_image_send(&self) {
        self.message_images.cancel_send();
    }

    /// Cancel image work owned by the current session view.
    #[wasm_bindgen(js_name = cancelMessageImages)]
    pub fn cancel_message_images(&self) {
        self.message_images.cancel_all();
    }
}

impl WaltrClient {
    fn binary_error(&self, error: ClientError) -> JsValue {
        if error.is_waltr_login_required() {
            self.message_images.suspend_for_access_change();
            drop(self.commands.unbounded_send(ClientCommandRequest::new(
                ClientCommand::WaltrAccessExpired {
                    error: "Waltr login required".into(),
                },
            )));
        }
        js_error(error)
    }
}

fn decode_parts(parts: &JsValue) -> Result<Vec<MessageInputPart>, JsValue> {
    let invalid = || js_error("Invalid message image parts");
    let count = parts_length(parts).map_err(|_| invalid())?;
    let count = u32::try_from(count).map_err(|_| invalid())?;
    if count as usize > MAX_MESSAGE_PARTS {
        return Err(invalid());
    }
    let mut result = Vec::with_capacity(count as usize);
    let mut text_bytes = 0;
    let mut image_bytes_total = 0;
    let mut image_count = 0;
    for index in 0..count {
        let field = |key| {
            part_field(parts, index, key)
                .ok()
                .and_then(|value| value.as_string())
                .ok_or_else(invalid)
        };
        match field("kind")?.as_str() {
            "text" => {
                let text = field("text")?;
                text_bytes += text.len();
                if text_bytes > MAX_MESSAGE_TEXT_BYTES {
                    return Err(js_error("Message text exceeds 16 KiB"));
                }
                result.push(MessageInputPart::Text(text));
            }
            "image" => {
                let format = match field("format")?.as_str() {
                    "png" => MessageImageFormatView::Png,
                    "jpeg" => MessageImageFormatView::Jpeg,
                    "webp" => MessageImageFormatView::Webp,
                    _ => return Err(invalid()),
                };
                let bytes = image_bytes(parts, index).map_err(|_| invalid())?;
                let len = image_length(&bytes).map_err(|_| invalid())?;
                if !(1.0..=MAX_MESSAGE_IMAGE_BYTES as f64).contains(&len) || len.fract() != 0.0 {
                    return Err(js_error("Each image must contain between 1 byte and 8 MiB"));
                }
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "validated integral image length"
                )]
                let len = len as u32;
                image_count += 1;
                image_bytes_total += len as usize;
                if image_count > MAX_MESSAGE_IMAGE_COUNT
                    || image_bytes_total > MAX_MESSAGE_TOTAL_IMAGE_BYTES
                {
                    return Err(js_error(
                        "A message can contain up to 4 images totaling 16 MiB",
                    ));
                }
                let bytes = copy_image(&bytes, len).map_err(|_| invalid())?;
                result.push(MessageInputPart::Image { format, bytes });
            }
            _ => return Err(invalid()),
        }
    }
    Ok(result)
}
