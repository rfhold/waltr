use super::*;
use crate::{
    remote_routes::IMAGE_UPLOAD_ROUTE_HEADER,
    session::{
        image_objects::ImageObjectError,
        image_uploads::{ImageUploadError, ImageUploadId, ImageUploadScope},
    },
};
use waltr::grpc::waltr::session::v1::{
    MessageImageFormat as WireFormat, UserMessageInputPart, user_message_input_part::Part,
};
use waltr_agent::{
    MessageImageDescriptor, MessageImageFormat, MessageImageLoadError, MessageImageLoadFuture,
    MessageImageLocation, MessageImageReference, MessageImageResolver, SessionRecord,
    TimelineItemId, TransientAttachmentBuffer, UserMessage, UserMessagePart,
    has_user_message_content,
};

const IMAGE_CHUNK_BYTES: usize = 64 * 1024;
type ImageStream = Pin<Box<dyn Stream<Item = Result<ReadMessageImageResponse, Status>> + Send>>;

pub(super) struct SavedImageResolver {
    objects: SessionImageObjects,
    slots: tokio::sync::Semaphore,
}

impl SavedImageResolver {
    pub(super) fn new(objects: SessionImageObjects) -> Self {
        Self {
            objects,
            slots: tokio::sync::Semaphore::new(8),
        }
    }
}

impl MessageImageResolver for SavedImageResolver {
    fn load<'a>(
        &'a self,
        _location: MessageImageLocation<'a>,
        image: MessageImageReference,
        target: &'a mut TransientAttachmentBuffer,
    ) -> MessageImageLoadFuture<'a> {
        Box::pin(async move {
            let MessageImageReference::Saved(image) = image else {
                return Err(MessageImageLoadError::Missing);
            };
            let _permit = self
                .slots
                .try_acquire()
                .map_err(|_| MessageImageLoadError::Unavailable)?;
            let bytes = self.objects.get(image).await.map_err(|error| match error {
                ImageObjectError::Missing => MessageImageLoadError::Missing,
                ImageObjectError::Invalid | ImageObjectError::Corrupt => {
                    MessageImageLoadError::Corrupt
                }
                ImageObjectError::Capacity | ImageObjectError::Unavailable => {
                    MessageImageLoadError::Unavailable
                }
            })?;
            target
                .append(&bytes)
                .map_err(|_| MessageImageLoadError::Corrupt)
        })
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> SessionService<S> {
    pub(super) fn ensure_image_upload_reaper(&self) {
        self.image_reaper.call_once(|| {
            let uploads = Arc::downgrade(&self.image_uploads);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    let Some(uploads) = uploads.upgrade() else {
                        break;
                    };
                    if uploads.reap_expired().is_err() {
                        tracing::warn!("message image staging unavailable");
                        break;
                    }
                }
            });
        });
    }

    pub(super) async fn image_record(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &SessionId,
    ) -> Result<SessionRecord, Status> {
        authorized_record(&self.sessions, &self.runtime_factory, principal, id).await
    }

    pub(super) async fn image_upload_scope<T: Sync>(
        &self,
        request: &Request<T>,
        session_id: &str,
    ) -> Result<(ImageUploadScope, String), Status> {
        let anonymous = AuthenticatedPrincipal::anonymous_disabled();
        let principal = request
            .extensions()
            .get::<AuthenticatedPrincipal>()
            .unwrap_or(&anonymous);
        let id = required_session_id(session_id)?;
        let record = self.image_record(principal, &id).await?;
        let incarnation = record
            .metadata
            .incarnation
            .ok_or_else(|| Status::failed_precondition("session has no current incarnation"))?;
        let route = match request.metadata().get(IMAGE_UPLOAD_ROUTE_HEADER) {
            None => "direct".to_owned(),
            Some(marker) => {
                let marker = marker
                    .to_str()
                    .ok()
                    .filter(|value| {
                        value.len() == 64
                            && value
                                .bytes()
                                .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
                    })
                    .ok_or_else(|| Status::invalid_argument("invalid image upload route"))?;
                if !principal.is_native_bearer() {
                    return Err(Status::permission_denied("image upload route unavailable"));
                }
                marker.to_owned()
            }
        };
        // Length framing keeps the exact issuer/subject pair unambiguous.
        let identity = format!(
            "{}:{}{}",
            principal.issuer().len(),
            principal.issuer(),
            principal.subject()
        );
        Ok((
            ImageUploadScope::new(identity, route, id, incarnation.clone()),
            incarnation,
        ))
    }

    pub(super) async fn read_visible_message_image(
        &self,
        request: Request<ReadMessageImageRequest>,
    ) -> Result<Response<ImageStream>, Status> {
        let principal = request
            .extensions()
            .get::<AuthenticatedPrincipal>()
            .cloned()
            .unwrap_or_else(AuthenticatedPrincipal::anonymous_disabled);
        let input = request.into_inner();
        let id = required_session_id(&input.session_id)?;
        let item = parse_item_id(&input.item_id)?;
        let index = input.part_index as usize;
        let permit = self
            .image_read_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("image read capacity exceeded"))?;
        let record = self.image_record(&principal, &id).await?;
        let descriptor = visible_image(&record, &item, index)?;
        let incarnation = record
            .metadata
            .incarnation
            .ok_or_else(|| Status::failed_precondition("session has no current incarnation"))?;
        let bytes = tokio::select! {
            biased;
            () = principal.access_ended() => return Err(Status::unauthenticated("access ended")),
            result = self.image_objects.get(descriptor) => result.map_err(object_status)?,
        };
        let sessions = self.sessions.clone();
        let factory = self.runtime_factory.clone();
        let authorize_chunk = move || {
            let (sessions, factory, principal, id, item, incarnation) = (
                sessions.clone(),
                factory.clone(),
                principal.clone(),
                id.clone(),
                item.clone(),
                incarnation.clone(),
            );
            async move {
                let current = authorized_record(&sessions, &factory, &principal, &id).await?;
                if current.metadata.incarnation.as_deref() != Some(incarnation.as_str())
                    || visible_image(&current, &item, index)? != descriptor
                {
                    return Err(Status::not_found("message image unavailable"));
                }
                Ok(())
            }
        };
        let stream = futures_util::stream::try_unfold(
            (bytes, 0, permit, authorize_chunk),
            |(bytes, offset, permit, authorize_chunk)| async move {
                authorize_chunk().await?;
                if offset == bytes.len() {
                    return Ok(None);
                }
                let end = bytes.len().min(offset + IMAGE_CHUNK_BYTES);
                let response = ReadMessageImageResponse {
                    data: bytes[offset..end].to_vec(),
                };
                Ok(Some((response, (bytes, end, permit, authorize_chunk))))
            },
        );
        Ok(Response::new(Box::pin(stream)))
    }
}

async fn authorized_record<S: SessionStore + Clone + Send + Sync + 'static>(
    sessions: &SessionCoordinator<S>,
    factory: &AgentSessionRuntimeFactory,
    principal: &AuthenticatedPrincipal,
    id: &SessionId,
) -> Result<SessionRecord, Status> {
    let access = async {
        let record = sessions
            .store()
            .load(id)
            .await
            .map_err(|error| match error {
                waltr_agent::SessionStoreError::NotFound { .. } => {
                    Status::not_found("session unavailable")
                }
                _ => Status::unavailable("session storage unavailable"),
            })?;
        factory
            .clone()
            .with_workspace_account(WorkspaceAccount::from_principal(principal))
            .authorize_session_workspace(&record)
            .await?;
        Ok(record)
    };
    tokio::select! {
        biased;
        () = principal.access_ended() => Err(Status::unauthenticated("access ended")),
        result = access => result,
    }
}

fn parse_item_id(value: &str) -> Result<TimelineItemId, Status> {
    value
        .strip_prefix('i')
        .filter(|value| !value.starts_with('0'))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|cursor| *cursor > 0)
        .map(|cursor| TimelineItemId::from_first_cursor(TimelineCursor::new(cursor)))
        .ok_or_else(|| Status::invalid_argument("invalid message item"))
}

fn visible_image(
    record: &SessionRecord,
    id: &TimelineItemId,
    index: usize,
) -> Result<MessageImageDescriptor, Status> {
    record
        .effective_user_message(id)
        .and_then(|message| message.parts().get(index))
        .and_then(|part| match part {
            UserMessagePart::Image {
                image: MessageImageReference::Saved(image),
            } => Some(*image),
            _ => None,
        })
        .ok_or_else(|| Status::not_found("message image unavailable"))
}

pub(super) fn object_status(error: ImageObjectError) -> Status {
    match error {
        ImageObjectError::Invalid => Status::invalid_argument("invalid message image"),
        ImageObjectError::Missing | ImageObjectError::Corrupt => {
            Status::data_loss("message image bytes could not be verified")
        }
        ImageObjectError::Capacity => Status::resource_exhausted("image storage capacity exceeded"),
        ImageObjectError::Unavailable => Status::unavailable("image storage unavailable"),
    }
}

pub(super) fn format_from_wire(format: i32) -> Result<MessageImageFormat, Status> {
    match WireFormat::try_from(format) {
        Ok(WireFormat::Png) => Ok(MessageImageFormat::Png),
        Ok(WireFormat::Jpeg) => Ok(MessageImageFormat::Jpeg),
        Ok(WireFormat::Webp) => Ok(MessageImageFormat::WebP),
        _ => Err(Status::invalid_argument("unsupported image format")),
    }
}

pub(super) fn upload_id(value: &str) -> Result<ImageUploadId, Status> {
    ImageUploadId::parse(value).ok_or_else(|| Status::not_found("image upload unavailable"))
}

pub(super) fn message_parts(
    parts: Vec<UserMessageInputPart>,
) -> Result<Vec<UserMessagePart<ImageUploadId>>, Status> {
    if parts.len() > 32 {
        return Err(Status::invalid_argument("too many message parts"));
    }
    let parts = parts
        .into_iter()
        .map(|part| match part.part {
            Some(Part::Text(text)) => Ok(UserMessagePart::Text { text }),
            Some(Part::ImageUploadId(id)) => Ok(UserMessagePart::Image {
                image: upload_id(&id)?,
            }),
            None => Err(Status::invalid_argument("message part is unspecified")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !has_user_message_content(&parts) {
        return Err(Status::invalid_argument(
            "message must contain text or an image",
        ));
    }
    Ok(parts)
}

pub(super) fn saved_message(
    parts: Vec<UserMessagePart<MessageImageDescriptor>>,
) -> Result<UserMessage, Status> {
    UserMessage::new(
        parts
            .into_iter()
            .map(|part| match part {
                UserMessagePart::Text { text } => UserMessagePart::Text { text },
                UserMessagePart::Image { image } => UserMessagePart::Image {
                    image: MessageImageReference::Saved(image),
                },
            })
            .collect(),
    )
    .map_err(|_| Status::invalid_argument("invalid message content"))
}

pub(super) fn text_message(
    parts: Vec<UserMessagePart<ImageUploadId>>,
) -> Result<UserMessage, Status> {
    let parts = parts
        .into_iter()
        .map(|part| match part {
            UserMessagePart::Text { text } => Ok(UserMessagePart::Text { text }),
            UserMessagePart::Image { .. } => Err(Status::invalid_argument("unexpected image")),
        })
        .collect::<Result<_, _>>()?;
    saved_message(parts)
}

pub(super) fn upload_status(error: ImageUploadError) -> Status {
    match error {
        ImageUploadError::Invalid => {
            Status::invalid_argument("invalid image upload or message parts")
        }
        ImageUploadError::Missing => Status::not_found("image upload unavailable"),
        ImageUploadError::Expired => Status::failed_precondition("image upload expired"),
        ImageUploadError::Conflict => {
            Status::failed_precondition("image upload not in the required stage")
        }
        ImageUploadError::Capacity => Status::resource_exhausted("image upload capacity exceeded"),
        ImageUploadError::Unavailable => Status::unavailable("image upload unavailable"),
    }
}
