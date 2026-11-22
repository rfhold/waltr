use crate::routine::ports::{HelperContent, PortError};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use waltr_agent::{
    MessageImageDescriptor, MessageImageLoadError, MessageImageLoadFuture, MessageImageLocation,
    MessageImageReference, MessageImageResolver, SessionId, SessionRecord, TimelineContent,
    TimelineItemId, TransientAttachment, TransientAttachmentBuffer, UserMessage, UserMessagePart,
};

type Sources = Vec<(MessageImageDescriptor, TransientAttachment)>;

pub(super) struct PreparedMessage {
    pub message: UserMessage,
    sources: Sources,
}

pub(super) fn prepare(
    mut content: HelperContent,
    prefix: Option<&str>,
) -> Result<PreparedMessage, PortError> {
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
    let mut sources = Vec::new();
    let parts = content
        .into_iter()
        .map(|part| match part {
            UserMessagePart::Text { text } => Ok(UserMessagePart::Text { text }),
            UserMessagePart::Image { image } => {
                let descriptor = image.image_descriptor().ok_or(PortError::Invalid)?;
                sources.push((descriptor, image));
                Ok(UserMessagePart::Image {
                    image: MessageImageReference::Invocation(descriptor),
                })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PreparedMessage {
        message: UserMessage::new(parts).map_err(|_| PortError::Invalid)?,
        sources,
    })
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

pub(super) struct LiveImages {
    id: SessionId,
    incarnation: Option<String>,
    saved: Option<Arc<dyn MessageImageResolver>>,
    copied_live_items: BTreeSet<TimelineItemId>,
    sources: Mutex<Option<Sources>>,
}

impl LiveImages {
    pub(super) fn new(
        record: &SessionRecord,
        saved: Option<Arc<dyn MessageImageResolver>>,
        initial: PreparedMessage,
    ) -> Self {
        // The last canonical user item is the helper's opening message. Copied live
        // items have no authority in the new invocation, even if a digest matches.
        let mut copied_live_items = BTreeSet::new();
        let mut last_user_item = None;
        for event in record.timeline_events() {
            if let TimelineContent::UserMessage { message } = &event.content {
                last_user_item = Some(&event.item_id);
                if message.parts().iter().any(|part| {
                    matches!(
                        part,
                        UserMessagePart::Image {
                            image: MessageImageReference::Invocation(_)
                        }
                    )
                }) {
                    copied_live_items.insert(event.item_id.clone());
                }
            }
        }
        if let Some(item) = last_user_item {
            copied_live_items.remove(item);
        }
        Self {
            id: record.id.clone(),
            incarnation: record.metadata.incarnation.clone(),
            saved,
            copied_live_items,
            sources: Mutex::new(Some(initial.sources)),
        }
    }

    pub(super) fn retain(
        &self,
        pending: PreparedMessage,
    ) -> Result<(UserMessage, PendingImages<'_>), PortError> {
        let mut live = self.sources.lock().map_err(|_| PortError::Unavailable)?;
        let sources = live.as_mut().ok_or(PortError::Denied)?;
        let before = sources.len();
        let total = sources
            .iter()
            .chain(&pending.sources)
            .map(|(descriptor, _)| descriptor.bytes() as usize)
            .sum::<usize>();
        if total > waltr_agent::MAX_TRANSIENT_SOURCE_BYTES {
            return Err(PortError::Invalid);
        }
        sources.extend(pending.sources);
        Ok((
            pending.message,
            PendingImages {
                images: self,
                before: Some(before),
            },
        ))
    }
}

pub(super) struct PendingImages<'a> {
    images: &'a LiveImages,
    before: Option<usize>,
}
impl PendingImages<'_> {
    pub(super) fn accepted(mut self) {
        self.before = None;
    }
}
impl Drop for PendingImages<'_> {
    fn drop(&mut self) {
        if let Some(before) = self.before
            && let Some(sources) = self
                .images
                .sources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
        {
            sources.truncate(before);
        }
    }
}

impl MessageImageResolver for LiveImages {
    fn load<'a>(
        &'a self,
        location: MessageImageLocation<'a>,
        image: MessageImageReference,
        target: &'a mut TransientAttachmentBuffer,
    ) -> MessageImageLoadFuture<'a> {
        Box::pin(async move {
            if location.session_id != &self.id
                || location.incarnation != self.incarnation.as_deref()
            {
                return Err(MessageImageLoadError::Missing);
            }
            match image {
                MessageImageReference::Saved(_) => {
                    self.saved
                        .as_ref()
                        .ok_or(MessageImageLoadError::Missing)?
                        .load(location, image, target)
                        .await
                }
                MessageImageReference::Invocation(descriptor) => {
                    if self.copied_live_items.contains(location.item_id) {
                        return Err(MessageImageLoadError::Missing);
                    }
                    let attachment = self
                        .sources
                        .lock()
                        .map_err(|_| MessageImageLoadError::Unavailable)?
                        .as_ref()
                        .ok_or(MessageImageLoadError::Missing)?
                        .iter()
                        .find(|(candidate, _)| *candidate == descriptor)
                        .map(|(_, source)| source.clone())
                        .ok_or(MessageImageLoadError::Missing)?;
                    attachment
                        .copy_into(target)
                        .map_err(|_| MessageImageLoadError::Corrupt)
                }
            }
        })
    }
}

pub(super) struct ImageLifetime(pub Arc<LiveImages>);
impl Drop for ImageLifetime {
    fn drop(&mut self) {
        self.0
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}
