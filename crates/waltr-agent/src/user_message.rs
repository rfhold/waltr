//! Provider-neutral content shared by message admission and replay.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// Retention and integrity metadata. Neither variant grants read access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "retention", content = "descriptor", rename_all = "camelCase")]
pub enum MessageImageReference {
    /// Immutable bytes retained with the saved conversation.
    Saved(MessageImageDescriptor),
    /// Bytes resolvable only by the owning live invocation, never by storage replay.
    Invocation(MessageImageDescriptor),
}

impl MessageImageReference {
    /// Expected format, length and digest.
    #[must_use]
    pub const fn descriptor(self) -> MessageImageDescriptor {
        match self {
            Self::Saved(descriptor) | Self::Invocation(descriptor) => descriptor,
        }
    }
}

/// An ordered user message. No attachment bytes are held in this type.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<UserMessagePart<MessageImageReference>>")]
pub struct UserMessage(Vec<UserMessagePart<MessageImageReference>>);

impl std::fmt::Debug for UserMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserMessage")
            .field("part_count", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl UserMessage {
    /// Validate ordered content before admission.
    ///
    /// # Errors
    /// Rejects blank, empty or oversized part lists or invalid image sizes.
    pub fn new(
        parts: Vec<UserMessagePart<MessageImageReference>>,
    ) -> Result<Self, UserMessageError> {
        Self::try_from(parts)
    }

    /// Exact, ordered parts.
    #[must_use]
    pub fn parts(&self) -> &[UserMessagePart<MessageImageReference>] {
        &self.0
    }

    /// Concatenated text, for text-only consumers. Never used to identify a message.
    #[must_use]
    pub fn text(&self) -> String {
        self.0
            .iter()
            .filter_map(|part| match part {
                UserMessagePart::Text { text } => Some(text.as_str()),
                UserMessagePart::Image { .. } => None,
            })
            .collect()
    }

    pub(crate) fn text_bytes(&self) -> usize {
        self.0.iter().fold(0usize, |len, part| match part {
            UserMessagePart::Text { text } => len.saturating_add(text.len()),
            UserMessagePart::Image { .. } => len,
        })
    }

    pub(crate) fn from_text(text: String) -> Result<Self, UserMessageError> {
        Self::new(vec![UserMessagePart::Text { text }])
    }

    pub(crate) fn into_parts(self) -> Vec<UserMessagePart<MessageImageReference>> {
        self.0
    }
}

impl TryFrom<Vec<UserMessagePart<MessageImageReference>>> for UserMessage {
    type Error = UserMessageError;

    fn try_from(parts: Vec<UserMessagePart<MessageImageReference>>) -> Result<Self, Self::Error> {
        if parts.len() > 32 || !has_user_message_content(&parts) {
            return Err(UserMessageError);
        }
        let mut images = 0;
        let mut image_bytes = 0;
        for part in &parts {
            if let UserMessagePart::Image { image } = part {
                let size = image.descriptor().bytes() as usize;
                if size == 0 || size > crate::MAX_TRANSIENT_ATTACHMENT_BYTES {
                    return Err(UserMessageError);
                }
                images += 1;
                image_bytes += size;
            }
        }
        if images > 4 || image_bytes > crate::MAX_TRANSIENT_SOURCE_BYTES {
            return Err(UserMessageError);
        }
        Ok(Self(parts))
    }
}

/// Invalid ordered user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserMessageError;

impl std::fmt::Display for UserMessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid user message content or bounds")
    }
}

impl std::error::Error for UserMessageError {}

/// An image position from the effective, committed conversation.
#[derive(Debug, Clone, Copy)]
pub struct MessageImageLocation<'a> {
    /// Session executing the request (the destination after a copy).
    pub session_id: &'a crate::SessionId,
    /// Persisted incarnation, absent on old records.
    pub incarnation: Option<&'a str>,
    /// The user timeline item that contains the reference.
    pub item_id: &'a crate::TimelineItemId,
    /// Zero-based position in the ordered message, including text parts.
    pub part_index: usize,
}

/// Redacted failure categories for host image resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageImageLoadError {
    /// Image is missing or not available to this invocation.
    Missing,
    /// Bytes failed verification.
    Corrupt,
    /// Backend could not verify or complete the read.
    Unavailable,
}

/// Future returned by a host image resolver.
pub type MessageImageLoadFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), MessageImageLoadError>> + Send + 'a>,
>;

/// Runtime-bound host loader; only the host knows its retention and access policy.
pub trait MessageImageResolver: Send + Sync {
    /// Append the source into a precharged bounded buffer. The agent independently
    /// checks the exact length, format signature, and digest before inference.
    fn load<'a>(
        &'a self,
        location: MessageImageLocation<'a>,
        image: MessageImageReference,
        target: &'a mut crate::TransientAttachmentBuffer,
    ) -> MessageImageLoadFuture<'a>;
}

pub(crate) fn text_part(api: crate::TransientInputApi, text: &str) -> Value {
    let kind = match api {
        crate::TransientInputApi::Anthropic | crate::TransientInputApi::OpenAiChat => "text",
        crate::TransientInputApi::OpenAiResponses => "input_text",
    };
    json!({"type": kind, "text": text})
}

/// A still image format supported by Waltr's inference encoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageImageFormat {
    /// Portable Network Graphics.
    Png,
    /// Joint Photographic Experts Group.
    Jpeg,
    /// WebP.
    WebP,
}

impl MessageImageFormat {
    /// Resolve an exact media type; never infer it from a filename.
    #[must_use]
    pub fn from_media_type(value: &str) -> Option<Self> {
        match value {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::WebP),
            _ => None,
        }
    }

    /// Media type used on the wire.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
        }
    }

    /// Check the format signature, not the full file.
    #[must_use]
    pub fn matches_signature(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(&[0xff, 0xd8, 0xff]),
            Self::WebP => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        }
    }
}

/// An ordered part of a user message. The image representation belongs to the
/// boundary that uses it, such as a durable reference or verified request bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum UserMessagePart<I> {
    /// Exact UTF-8 text.
    Text {
        /// The text at this position.
        text: String,
    },
    /// An image at this position.
    Image {
        /// The image representation for this boundary.
        image: I,
    },
}

/// Integrity metadata for a saved message image. It does not grant read access.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageImageDescriptor {
    format: MessageImageFormat,
    bytes: u32,
    sha256: [u8; 32],
}

impl std::fmt::Debug for MessageImageDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageImageDescriptor")
            .field("format", &self.format)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl MessageImageDescriptor {
    /// Validate the byte bound and format signature before deriving integrity metadata.
    #[must_use]
    pub fn for_bytes(format: MessageImageFormat, bytes: &[u8]) -> Option<Self> {
        if bytes.len() > crate::MAX_TRANSIENT_ATTACHMENT_BYTES || !format.matches_signature(bytes) {
            return None;
        }
        Some(Self {
            format,
            bytes: u32::try_from(bytes.len()).ok()?,
            sha256: Sha256::digest(bytes).into(),
        })
    }

    /// Bytes to expect on a verified read.
    #[must_use]
    pub const fn bytes(self) -> u32 {
        self.bytes
    }

    /// Format declared by the producer and checked against the bytes.
    #[must_use]
    pub const fn format(self) -> MessageImageFormat {
        self.format
    }

    /// Integrity digest. It is not a public identifier or access credential.
    #[must_use]
    pub const fn sha256(self) -> [u8; 32] {
        self.sha256
    }

    /// Reject an unexpected size, invalid signature, or wrong digest.
    #[must_use]
    pub fn verifies(self, bytes: &[u8]) -> bool {
        bytes.len() == self.bytes as usize && Self::for_bytes(self.format, bytes) == Some(self)
    }
}

/// Check that an ordered message has at least one substantive part.
#[must_use]
pub fn has_user_message_content<I>(parts: &[UserMessagePart<I>]) -> bool {
    parts.iter().any(|part| match part {
        UserMessagePart::Text { text } => !text.trim().is_empty(),
        UserMessagePart::Image { .. } => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_parts_preserve_order_and_allow_image_only_messages() {
        let parts: Vec<UserMessagePart<String>> = serde_json::from_value(serde_json::json!([
            {"type": "text", "text": "before"},
            {"type": "image", "image": "opaque"},
            {"type": "text", "text": "after"}
        ]))
        .unwrap();
        assert!(matches!(&parts[0], UserMessagePart::Text { text } if text == "before"));
        assert!(has_user_message_content(&parts[1..2]));
        assert!(matches!(&parts[2], UserMessagePart::Text { text } if text == "after"));
        assert!(!has_user_message_content::<String>(&[]));
        assert!(!has_user_message_content::<String>(&[
            UserMessagePart::Text { text: " \n".into() }
        ]));
    }

    #[test]
    fn image_descriptor_authenticates_format_size_and_digest() {
        let png = b"\x89PNG\r\n\x1a\nexample";
        let descriptor = MessageImageDescriptor::for_bytes(MessageImageFormat::Png, png).unwrap();
        assert!(descriptor.verifies(png));
        assert!(!descriptor.verifies(b"\x89PNG\r\n\x1a\nchanged"));
        assert!(!descriptor.verifies(&png[..png.len() - 1]));
        assert!(MessageImageDescriptor::for_bytes(MessageImageFormat::Jpeg, png).is_none());
    }
}
