use crate::{
    routine::ports::{HelperContent, PortError},
    session::image_objects::{ImageObjectError, SessionImageObjects},
};
use waltr_agent::{MessageImageReference, UserMessage, UserMessagePart};

pub(super) async fn prepare(
    mut content: HelperContent,
    prefix: Option<&str>,
    objects: Option<&SessionImageObjects>,
    identity: Option<&str>,
) -> Result<UserMessage, PortError> {
    if !valid_content(&content) {
        return Err(PortError::Invalid);
    }
    if let Some(prefix) = prefix {
        match &mut content[0] {
            UserMessagePart::Text { text } => text.insert_str(0, &format!("{prefix}\n")),
            UserMessagePart::Image { .. } if !prefix.is_empty() => content.insert(
                0,
                UserMessagePart::Text {
                    text: format!("{prefix}\n"),
                },
            ),
            UserMessagePart::Image { .. } => {}
        }
    }
    let mut bytes = 0u64;
    for part in &content {
        if let UserMessagePart::Image { image } = part {
            bytes += u64::from(image.image_descriptor().ok_or(PortError::Invalid)?.bytes());
        }
    }
    if content.len() > 32 || bytes > waltr_agent::MAX_TRANSIENT_SOURCE_BYTES as u64 {
        return Err(PortError::Invalid);
    }
    let mut reservation = if bytes > 0 {
        let objects = objects.ok_or(PortError::UnsupportedCapability)?;
        let identity = identity.ok_or(PortError::Denied)?;
        Some(objects.reserve(identity, bytes).await.map_err(object_error)?)
    } else {
        None
    };
    let mut parts = Vec::with_capacity(content.len());
    for part in content {
        parts.push(match part {
            UserMessagePart::Text { text } => UserMessagePart::Text { text },
            UserMessagePart::Image { image } => {
                let (format, bytes) = image.image_source().ok_or(PortError::Invalid)?;
                let descriptor = reservation
                    .as_mut()
                    .ok_or(PortError::Invalid)?
                    .put(format, bytes)
                    .await
                    .map_err(object_error)?;
                UserMessagePart::Image {
                    image: MessageImageReference::Saved(descriptor),
                }
            }
        });
    }
    UserMessage::new(parts).map_err(|_| PortError::Invalid)
}

fn object_error(error: ImageObjectError) -> PortError {
    match error {
        ImageObjectError::Invalid => PortError::Invalid,
        ImageObjectError::Capacity => PortError::Busy,
        _ => PortError::Unavailable,
    }
}

pub(crate) fn valid_content<I>(content: &[UserMessagePart<I>]) -> bool {
    content.len() <= 32
        && waltr_agent::has_user_message_content(content)
        && content
            .iter()
            .filter(|part| matches!(part, UserMessagePart::Image { .. }))
            .count()
            <= 4
        && content
            .iter()
            .filter_map(|part| match part {
                UserMessagePart::Text { text } => Some(text.len()),
                UserMessagePart::Image { .. } => None,
            })
            .sum::<usize>()
            <= 16384
}
