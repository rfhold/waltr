use crate::support::grpc::session_client;
use tonic::Code;
use waltr::grpc::waltr::session::v1::{
    AbortMessageImageUploadRequest, BeginMessageImageUploadRequest, CreateSessionRequest,
    MessageImageFormat, ReadMessageImageRequest, SealMessageImageUploadRequest,
    WriteMessageImageChunkRequest,
};
use waltr_server::test_support::setup_server;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";

#[tokio::test]
async fn staged_image_is_session_scoped_and_not_readable() -> Result<(), Box<dyn std::error::Error>>
{
    waltr_server::isolate_application_test!();
    let mut client = session_client(setup_server()).await?;
    let first = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .unwrap()
        .id;
    let other = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .unwrap()
        .id;
    let upload_id = client
        .begin_message_image_upload(BeginMessageImageUploadRequest {
            session_id: first.clone(),
            format: MessageImageFormat::Png as i32,
            size_bytes: u32::try_from(PNG.len())?,
        })
        .await?
        .into_inner()
        .upload_id;
    let mut write = WriteMessageImageChunkRequest {
        session_id: other,
        upload_id: upload_id.clone(),
        offset: 0,
        data: PNG.to_vec(),
    };
    assert_eq!(
        client
            .write_message_image_chunk(write.clone())
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    write.session_id.clone_from(&first);
    client.write_message_image_chunk(write.clone()).await?;
    client
        .seal_message_image_upload(SealMessageImageUploadRequest {
            session_id: first.clone(),
            upload_id: upload_id.clone(),
        })
        .await?;

    assert_eq!(
        client
            .read_message_image(ReadMessageImageRequest {
                session_id: first.clone(),
                item_id: "i1".into(),
                part_index: 0,
            })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );

    client
        .abort_message_image_upload(AbortMessageImageUploadRequest {
            session_id: first,
            upload_id,
        })
        .await?;
    assert_eq!(
        client
            .write_message_image_chunk(write)
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    Ok(())
}
