use super::*;
use crate::session::image_objects::SessionImageObjects;
use futures_util::StreamExt as _;
use tonic::Code;
use waltr::grpc::waltr::session::v1::{
    BeginMessageImageUploadRequest, DeleteSessionRequest, GetTimelineRequest,
    MessageImageFormat as WireFormat, ReadMessageImageRequest, SealMessageImageUploadRequest,
    SubmitUserMessageRequest, TimelineProjectionMode, UserMessageInputPart,
    WriteMessageImageChunkRequest, timeline_content, user_message_input_part,
    user_message_timeline_part,
};
use waltr_agent::{
    FilesystemSessionStore, MessageImageFormat, MessageImageReference, ReplacementUserContent,
    SessionCreateRequest, SessionJournalEvent, SessionMaskAddition, SessionMaskMutation,
    SessionMutation, SessionStart, SessionStore as _, TimelineCursor, TimelineItemId, UserMessage,
    UserMessagePart,
};

#[tokio::test]
async fn sealed_upload_is_committed_once_and_reaches_provider() {
    struct ImageProvider(Arc<tokio::sync::Notify>);
    impl waltr_agent::LlmStream for ImageProvider {
        fn name(&self) -> &'static str {
            "image-test"
        }

        fn transient_api(&self) -> Option<waltr_agent::TransientInputApi> {
            Some(waltr_agent::TransientInputApi::OpenAiChat)
        }

        fn preflight_transient_request(
            &self,
            _: &waltr_agent::CompletionRequest<'_>,
            _: &waltr_agent::AdmittedTransientInput,
        ) -> waltr_agent::Result<()> {
            Ok(())
        }

        fn stream_transient_request<'a>(
            &'a self,
            request: waltr_agent::CompletionRequest<'a>,
            _: &'a waltr_agent::AdmittedTransientInput,
        ) -> waltr_agent::BoxStreamFuture<'a> {
            assert_eq!(request.prompt, "look");
            self.0.notify_one();
            Box::pin(async {
                Ok(Box::pin(futures_util::stream::iter([
                    Ok(waltr_agent::CompletionEvent::Delta("saw image".into())),
                    Ok(waltr_agent::CompletionEvent::Finished),
                ])) as waltr_agent::BoxCompletionStream)
            })
        }

        fn stream_request<'a>(
            &'a self,
            _: waltr_agent::CompletionRequest<'a>,
        ) -> waltr_agent::BoxStreamFuture<'a> {
            panic!("must not drop the image and call the text path")
        }
    }

    let png = b"\x89PNG\r\n\x1a\nfixture";
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let catalog = SessionCatalogSource::Static(
        crate::agent_host::definition::AgentDefinitionCatalog::from_sources(
            vec![agent_source("image-test", "image fixture")],
            "image-test",
        )
        .unwrap(),
    );
    let (input, sandbox) = test_input(temp.path(), catalog).await;
    let reached_provider = Arc::new(tokio::sync::Notify::new());
    let service = Arc::try_unwrap(SessionService::from_sessions_with_images(
        SessionCoordinator::new(FilesystemSessionStore::new(&root)),
        input,
        SessionImageObjects::filesystem(&root).unwrap(),
    ))
    .unwrap()
    .with_test_provider(Arc::new(ImageProvider(reached_provider.clone())));
    let session_id = service
        .create_session(Request::new(CreateSessionRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .session
        .unwrap()
        .id;
    let upload_id = service
        .begin_message_image_upload(Request::new(BeginMessageImageUploadRequest {
            session_id: session_id.clone(),
            format: WireFormat::Png as i32,
            size_bytes: u32::try_from(png.len()).unwrap(),
        }))
        .await
        .unwrap()
        .into_inner()
        .upload_id;
    service
        .write_message_image_chunk(Request::new(WriteMessageImageChunkRequest {
            session_id: session_id.clone(),
            upload_id: upload_id.clone(),
            offset: 0,
            data: png.to_vec(),
        }))
        .await
        .unwrap();
    service
        .seal_message_image_upload(Request::new(SealMessageImageUploadRequest {
            session_id: session_id.clone(),
            upload_id: upload_id.clone(),
        }))
        .await
        .unwrap();
    let submit = SubmitUserMessageRequest {
        session_id: session_id.clone(),
        parts: vec![
            UserMessageInputPart::text("look"),
            UserMessageInputPart {
                part: Some(user_message_input_part::Part::ImageUploadId(upload_id)),
            },
        ],
    };
    service
        .submit_user_message(Request::new(submit.clone()))
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reached_provider.notified(),
    )
    .await
    .expect("committed image is dispatched to the provider");
    assert_eq!(
        service
            .submit_user_message(Request::new(submit))
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    let timeline = service
        .get_timeline(Request::new(GetTimelineRequest {
            session_id: session_id.clone(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    let mut user_items = timeline.events.iter().filter(|event| {
        matches!(
            event
                .content
                .as_ref()
                .and_then(|content| content.content.as_ref()),
            Some(timeline_content::Content::UserMessage(_))
        )
    });
    let saved = user_items.next().unwrap().item_id.clone();
    assert!(user_items.next().is_none());
    let mut stream = service
        .read_message_image(Request::new(ReadMessageImageRequest {
            session_id,
            item_id: saved,
            part_index: 1,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.next().await.unwrap().unwrap().data, png);
    assert!(stream.next().await.is_none());
    drop(stream);
    sandbox.shutdown().await;
}

#[tokio::test]
async fn saved_images_reopen_and_require_a_current_message_reference() {
    const CHUNK: usize = 64 * 1024;
    let mut png = vec![0; CHUNK + 1];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let temp = tempfile::tempdir().unwrap();
    let store_root = temp.path().join("sessions");
    let session_id = SessionId::generate();
    {
        let objects = SessionImageObjects::filesystem(&store_root).unwrap();
        let descriptor = objects.put(MessageImageFormat::Png, &png).await.unwrap();
        let mut create = SessionCreateRequest::new(session_id.clone());
        create.start = SessionStart::Fresh {
            messages: vec![
                UserMessage::new(vec![
                    UserMessagePart::Text {
                        text: "look".into(),
                    },
                    UserMessagePart::Image {
                        image: MessageImageReference::Saved(descriptor),
                    },
                ])
                .unwrap()
                .into(),
            ],
        };
        let store = FilesystemSessionStore::new(&store_root);
        let created = SessionCoordinator::new(store.clone())
            .create(create)
            .await
            .unwrap();
        store
            .append_journal(
                &session_id,
                created.record.execution_token().unwrap(),
                vec![SessionJournalEvent::TurnFinished { turn: 0 }],
            )
            .await
            .unwrap();
    }
    let catalog = SessionCatalogSource::Static(
        crate::agent_host::definition::AgentDefinitionCatalog::from_sources(
            vec![agent_source("image-test", "image fixture")],
            "image-test",
        )
        .unwrap(),
    );
    let (input, sandbox) = test_input(temp.path(), catalog).await;
    let store = FilesystemSessionStore::new(&store_root);
    let service = SessionService::from_sessions_with_images(
        SessionCoordinator::new(store.clone()),
        input,
        SessionImageObjects::filesystem(&store_root).unwrap(),
    );
    let image = ReadMessageImageRequest {
        session_id: session_id.as_str().into(),
        item_id: "i1".into(),
        part_index: 1,
    };
    let timeline = service
        .get_timeline(Request::new(GetTimelineRequest {
            session_id: session_id.as_str().into(),
            projection_mode: TimelineProjectionMode::Raw as i32,
            ..Default::default()
        }))
        .await
        .unwrap();
    let [event] = timeline.get_ref().events.as_slice() else {
        panic!("expected one durable timeline event");
    };
    let Some(timeline_content::Content::UserMessage(message)) = event
        .content
        .as_ref()
        .and_then(|content| content.content.as_ref())
    else {
        panic!("expected ordered user message content");
    };
    assert!(matches!(
        message.parts[1].part.as_ref(),
        Some(user_message_timeline_part::Part::Image(descriptor))
            if descriptor.format == WireFormat::Png as i32
                && descriptor.size_bytes as usize == png.len()
    ));
    let wrong_part = ReadMessageImageRequest {
        part_index: 0,
        ..image.clone()
    };
    assert_eq!(
        service
            .read_message_image(Request::new(wrong_part))
            .await
            .err()
            .unwrap()
            .code(),
        Code::NotFound
    );
    let mut stream = service
        .read_message_image(Request::new(image.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.next().await.unwrap().unwrap().data, png[..CHUNK]);
    assert_eq!(stream.next().await.unwrap().unwrap().data, png[CHUNK..]);
    assert!(stream.next().await.is_none());

    let mut stream = service
        .read_message_image(Request::new(image.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.next().await.unwrap().unwrap().data, png[..CHUNK]);
    let cursor = TimelineCursor::new(1);
    let mut masks = SessionMaskMutation::new(Some(cursor));
    masks.add.push(SessionMaskAddition::items(
        "hide-image",
        cursor,
        vec![TimelineItemId::from_first_cursor(cursor)],
        ReplacementUserContent::new("hidden"),
    ));
    store
        .mutate(
            &session_id,
            SessionMutation {
                masks: Some(masks),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap_err().code(),
        Code::NotFound
    );
    let mut masks = SessionMaskMutation::new(Some(cursor));
    masks.subtract.keys.push("hide-image".into());
    store
        .mutate(
            &session_id,
            SessionMutation {
                masks: Some(masks),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let mut stream = service
        .read_message_image(Request::new(image.clone()))
        .await
        .unwrap()
        .into_inner();
    service
        .delete_session(Request::new(DeleteSessionRequest {
            id: session_id.as_str().into(),
        }))
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap_err().code(),
        Code::NotFound
    );
    assert_eq!(
        service
            .read_message_image(Request::new(image))
            .await
            .err()
            .unwrap()
            .code(),
        Code::NotFound
    );
    sandbox.shutdown().await;
}
