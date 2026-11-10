// @generated
/// Generated client implementations.
pub mod routine_service_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct RoutineServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl<T> RoutineServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> RoutineServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::Body>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            RoutineServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn watch_definitions(
            &mut self,
            request: impl tonic::IntoRequest<super::WatchDefinitionsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::WatchDefinitionsResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/WatchDefinitions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.routine.v2.RoutineService",
                        "WatchDefinitions",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn start_invocation(
            &mut self,
            request: impl tonic::IntoRequest<super::StartInvocationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::StartInvocationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/StartInvocation",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "StartInvocation"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_invocations(
            &mut self,
            request: impl tonic::IntoRequest<super::ListInvocationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListInvocationsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/ListInvocations",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "ListInvocations"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_invocation(
            &mut self,
            request: impl tonic::IntoRequest<super::GetInvocationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetInvocationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/GetInvocation",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "GetInvocation"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn watch_invocations(
            &mut self,
            request: impl tonic::IntoRequest<super::WatchInvocationsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::WatchInvocationsResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/WatchInvocations",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.routine.v2.RoutineService",
                        "WatchInvocations",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn list_operations(
            &mut self,
            request: impl tonic::IntoRequest<super::ListOperationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListOperationsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/ListOperations",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "ListOperations"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn cancel_invocation(
            &mut self,
            request: impl tonic::IntoRequest<super::CancelInvocationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CancelInvocationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/CancelInvocation",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.routine.v2.RoutineService",
                        "CancelInvocation",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn read_text_result(
            &mut self,
            request: impl tonic::IntoRequest<super::ReadTextResultRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ReadTextResultResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/ReadTextResult",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "ReadTextResult"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn interact(
            &mut self,
            request: impl tonic::IntoStreamingRequest<Message = super::InteractRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::InteractResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/Interact",
            );
            let mut req = request.into_streaming_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("waltr.routine.v2.RoutineService", "Interact"));
            self.inner.streaming(req, path, codec).await
        }
        pub async fn open_attachment(
            &mut self,
            request: impl tonic::IntoRequest<super::OpenAttachmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::OpenAttachmentResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/OpenAttachment",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "OpenAttachment"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn send_input_batch(
            &mut self,
            request: impl tonic::IntoRequest<super::SendInputBatchRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendInputBatchResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/SendInputBatch",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "SendInputBatch"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn watch_live_output(
            &mut self,
            request: impl tonic::IntoRequest<super::WatchLiveOutputRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::WatchLiveOutputResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/WatchLiveOutput",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "WatchLiveOutput"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn renew_attachment(
            &mut self,
            request: impl tonic::IntoRequest<super::RenewAttachmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RenewAttachmentResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/RenewAttachment",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "RenewAttachment"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn close_attachment(
            &mut self,
            request: impl tonic::IntoRequest<super::CloseAttachmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CloseAttachmentResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/CloseAttachment",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "CloseAttachment"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn begin_blob_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::BeginBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::BeginBlobUploadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/BeginBlobUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "BeginBlobUpload"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn append_blob_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::AppendBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AppendBlobUploadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/AppendBlobUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.routine.v2.RoutineService",
                        "AppendBlobUpload",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn commit_blob_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::CommitBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CommitBlobUploadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/CommitBlobUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.routine.v2.RoutineService",
                        "CommitBlobUpload",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn cancel_blob_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::CancelBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CancelBlobUploadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/CancelBlobUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.routine.v2.RoutineService",
                        "CancelBlobUpload",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn open_blob_read(
            &mut self,
            request: impl tonic::IntoRequest<super::OpenBlobReadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::OpenBlobReadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/OpenBlobRead",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "OpenBlobRead"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn read_blob(
            &mut self,
            request: impl tonic::IntoRequest<super::ReadBlobRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ReadBlobResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/ReadBlob",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("waltr.routine.v2.RoutineService", "ReadBlob"));
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn close_blob_read(
            &mut self,
            request: impl tonic::IntoRequest<super::CloseBlobReadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CloseBlobReadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/waltr.routine.v2.RoutineService/CloseBlobRead",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.routine.v2.RoutineService", "CloseBlobRead"),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod routine_service_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with RoutineServiceServer.
    #[async_trait]
    pub trait RoutineService: std::marker::Send + std::marker::Sync + 'static {
        /// Server streaming response type for the WatchDefinitions method.
        type WatchDefinitionsStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<
                    super::WatchDefinitionsResponse,
                    tonic::Status,
                >,
            >
            + std::marker::Send
            + 'static;
        async fn watch_definitions(
            &self,
            request: tonic::Request<super::WatchDefinitionsRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::WatchDefinitionsStream>,
            tonic::Status,
        >;
        async fn start_invocation(
            &self,
            request: tonic::Request<super::StartInvocationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::StartInvocationResponse>,
            tonic::Status,
        >;
        async fn list_invocations(
            &self,
            request: tonic::Request<super::ListInvocationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListInvocationsResponse>,
            tonic::Status,
        >;
        async fn get_invocation(
            &self,
            request: tonic::Request<super::GetInvocationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetInvocationResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the WatchInvocations method.
        type WatchInvocationsStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<
                    super::WatchInvocationsResponse,
                    tonic::Status,
                >,
            >
            + std::marker::Send
            + 'static;
        async fn watch_invocations(
            &self,
            request: tonic::Request<super::WatchInvocationsRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::WatchInvocationsStream>,
            tonic::Status,
        >;
        async fn list_operations(
            &self,
            request: tonic::Request<super::ListOperationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListOperationsResponse>,
            tonic::Status,
        >;
        async fn cancel_invocation(
            &self,
            request: tonic::Request<super::CancelInvocationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CancelInvocationResponse>,
            tonic::Status,
        >;
        async fn read_text_result(
            &self,
            request: tonic::Request<super::ReadTextResultRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ReadTextResultResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the Interact method.
        type InteractStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::InteractResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn interact(
            &self,
            request: tonic::Request<tonic::Streaming<super::InteractRequest>>,
        ) -> std::result::Result<tonic::Response<Self::InteractStream>, tonic::Status>;
        async fn open_attachment(
            &self,
            request: tonic::Request<super::OpenAttachmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::OpenAttachmentResponse>,
            tonic::Status,
        >;
        async fn send_input_batch(
            &self,
            request: tonic::Request<super::SendInputBatchRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendInputBatchResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the WatchLiveOutput method.
        type WatchLiveOutputStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::WatchLiveOutputResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn watch_live_output(
            &self,
            request: tonic::Request<super::WatchLiveOutputRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::WatchLiveOutputStream>,
            tonic::Status,
        >;
        async fn renew_attachment(
            &self,
            request: tonic::Request<super::RenewAttachmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RenewAttachmentResponse>,
            tonic::Status,
        >;
        async fn close_attachment(
            &self,
            request: tonic::Request<super::CloseAttachmentRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CloseAttachmentResponse>,
            tonic::Status,
        >;
        async fn begin_blob_upload(
            &self,
            request: tonic::Request<super::BeginBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::BeginBlobUploadResponse>,
            tonic::Status,
        >;
        async fn append_blob_upload(
            &self,
            request: tonic::Request<super::AppendBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AppendBlobUploadResponse>,
            tonic::Status,
        >;
        async fn commit_blob_upload(
            &self,
            request: tonic::Request<super::CommitBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CommitBlobUploadResponse>,
            tonic::Status,
        >;
        async fn cancel_blob_upload(
            &self,
            request: tonic::Request<super::CancelBlobUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CancelBlobUploadResponse>,
            tonic::Status,
        >;
        async fn open_blob_read(
            &self,
            request: tonic::Request<super::OpenBlobReadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::OpenBlobReadResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the ReadBlob method.
        type ReadBlobStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::ReadBlobResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn read_blob(
            &self,
            request: tonic::Request<super::ReadBlobRequest>,
        ) -> std::result::Result<tonic::Response<Self::ReadBlobStream>, tonic::Status>;
        async fn close_blob_read(
            &self,
            request: tonic::Request<super::CloseBlobReadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CloseBlobReadResponse>,
            tonic::Status,
        >;
    }
    #[derive(Debug)]
    pub struct RoutineServiceServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> RoutineServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for RoutineServiceServer<T>
    where
        T: RoutineService,
        B: Body + std::marker::Send + 'static,
        B::Error: Into<StdError> + std::marker::Send + 'static,
    {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            match req.uri().path() {
                "/waltr.routine.v2.RoutineService/WatchDefinitions" => {
                    #[allow(non_camel_case_types)]
                    struct WatchDefinitionsSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::ServerStreamingService<
                        super::WatchDefinitionsRequest,
                    > for WatchDefinitionsSvc<T> {
                        type Response = super::WatchDefinitionsResponse;
                        type ResponseStream = T::WatchDefinitionsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::WatchDefinitionsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::watch_definitions(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = WatchDefinitionsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/StartInvocation" => {
                    #[allow(non_camel_case_types)]
                    struct StartInvocationSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::StartInvocationRequest>
                    for StartInvocationSvc<T> {
                        type Response = super::StartInvocationResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::StartInvocationRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::start_invocation(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = StartInvocationSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/ListInvocations" => {
                    #[allow(non_camel_case_types)]
                    struct ListInvocationsSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::ListInvocationsRequest>
                    for ListInvocationsSvc<T> {
                        type Response = super::ListInvocationsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListInvocationsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::list_invocations(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListInvocationsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/GetInvocation" => {
                    #[allow(non_camel_case_types)]
                    struct GetInvocationSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::GetInvocationRequest>
                    for GetInvocationSvc<T> {
                        type Response = super::GetInvocationResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetInvocationRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::get_invocation(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetInvocationSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/WatchInvocations" => {
                    #[allow(non_camel_case_types)]
                    struct WatchInvocationsSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::ServerStreamingService<
                        super::WatchInvocationsRequest,
                    > for WatchInvocationsSvc<T> {
                        type Response = super::WatchInvocationsResponse;
                        type ResponseStream = T::WatchInvocationsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::WatchInvocationsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::watch_invocations(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = WatchInvocationsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/ListOperations" => {
                    #[allow(non_camel_case_types)]
                    struct ListOperationsSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::ListOperationsRequest>
                    for ListOperationsSvc<T> {
                        type Response = super::ListOperationsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListOperationsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::list_operations(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListOperationsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/CancelInvocation" => {
                    #[allow(non_camel_case_types)]
                    struct CancelInvocationSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::CancelInvocationRequest>
                    for CancelInvocationSvc<T> {
                        type Response = super::CancelInvocationResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CancelInvocationRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::cancel_invocation(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CancelInvocationSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/ReadTextResult" => {
                    #[allow(non_camel_case_types)]
                    struct ReadTextResultSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::ReadTextResultRequest>
                    for ReadTextResultSvc<T> {
                        type Response = super::ReadTextResultResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ReadTextResultRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::read_text_result(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ReadTextResultSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/Interact" => {
                    #[allow(non_camel_case_types)]
                    struct InteractSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::StreamingService<super::InteractRequest>
                    for InteractSvc<T> {
                        type Response = super::InteractResponse;
                        type ResponseStream = T::InteractStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                tonic::Streaming<super::InteractRequest>,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::interact(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = InteractSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/OpenAttachment" => {
                    #[allow(non_camel_case_types)]
                    struct OpenAttachmentSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::OpenAttachmentRequest>
                    for OpenAttachmentSvc<T> {
                        type Response = super::OpenAttachmentResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::OpenAttachmentRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::open_attachment(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = OpenAttachmentSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/SendInputBatch" => {
                    #[allow(non_camel_case_types)]
                    struct SendInputBatchSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::SendInputBatchRequest>
                    for SendInputBatchSvc<T> {
                        type Response = super::SendInputBatchResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SendInputBatchRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::send_input_batch(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = SendInputBatchSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/WatchLiveOutput" => {
                    #[allow(non_camel_case_types)]
                    struct WatchLiveOutputSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::ServerStreamingService<
                        super::WatchLiveOutputRequest,
                    > for WatchLiveOutputSvc<T> {
                        type Response = super::WatchLiveOutputResponse;
                        type ResponseStream = T::WatchLiveOutputStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::WatchLiveOutputRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::watch_live_output(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = WatchLiveOutputSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/RenewAttachment" => {
                    #[allow(non_camel_case_types)]
                    struct RenewAttachmentSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::RenewAttachmentRequest>
                    for RenewAttachmentSvc<T> {
                        type Response = super::RenewAttachmentResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RenewAttachmentRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::renew_attachment(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = RenewAttachmentSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/CloseAttachment" => {
                    #[allow(non_camel_case_types)]
                    struct CloseAttachmentSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::CloseAttachmentRequest>
                    for CloseAttachmentSvc<T> {
                        type Response = super::CloseAttachmentResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CloseAttachmentRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::close_attachment(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CloseAttachmentSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/BeginBlobUpload" => {
                    #[allow(non_camel_case_types)]
                    struct BeginBlobUploadSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::BeginBlobUploadRequest>
                    for BeginBlobUploadSvc<T> {
                        type Response = super::BeginBlobUploadResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::BeginBlobUploadRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::begin_blob_upload(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = BeginBlobUploadSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/AppendBlobUpload" => {
                    #[allow(non_camel_case_types)]
                    struct AppendBlobUploadSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::AppendBlobUploadRequest>
                    for AppendBlobUploadSvc<T> {
                        type Response = super::AppendBlobUploadResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::AppendBlobUploadRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::append_blob_upload(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = AppendBlobUploadSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/CommitBlobUpload" => {
                    #[allow(non_camel_case_types)]
                    struct CommitBlobUploadSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::CommitBlobUploadRequest>
                    for CommitBlobUploadSvc<T> {
                        type Response = super::CommitBlobUploadResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CommitBlobUploadRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::commit_blob_upload(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CommitBlobUploadSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/CancelBlobUpload" => {
                    #[allow(non_camel_case_types)]
                    struct CancelBlobUploadSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::CancelBlobUploadRequest>
                    for CancelBlobUploadSvc<T> {
                        type Response = super::CancelBlobUploadResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CancelBlobUploadRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::cancel_blob_upload(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CancelBlobUploadSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/OpenBlobRead" => {
                    #[allow(non_camel_case_types)]
                    struct OpenBlobReadSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::OpenBlobReadRequest>
                    for OpenBlobReadSvc<T> {
                        type Response = super::OpenBlobReadResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::OpenBlobReadRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::open_blob_read(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = OpenBlobReadSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/ReadBlob" => {
                    #[allow(non_camel_case_types)]
                    struct ReadBlobSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::ServerStreamingService<super::ReadBlobRequest>
                    for ReadBlobSvc<T> {
                        type Response = super::ReadBlobResponse;
                        type ResponseStream = T::ReadBlobStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ReadBlobRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::read_blob(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ReadBlobSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/waltr.routine.v2.RoutineService/CloseBlobRead" => {
                    #[allow(non_camel_case_types)]
                    struct CloseBlobReadSvc<T: RoutineService>(pub Arc<T>);
                    impl<
                        T: RoutineService,
                    > tonic::server::UnaryService<super::CloseBlobReadRequest>
                    for CloseBlobReadSvc<T> {
                        type Response = super::CloseBlobReadResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CloseBlobReadRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as RoutineService>::close_blob_read(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CloseBlobReadSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        let mut response = http::Response::new(
                            tonic::body::Body::default(),
                        );
                        let headers = response.headers_mut();
                        headers
                            .insert(
                                tonic::Status::GRPC_STATUS,
                                (tonic::Code::Unimplemented as i32).into(),
                            );
                        headers
                            .insert(
                                http::header::CONTENT_TYPE,
                                tonic::metadata::GRPC_CONTENT_TYPE,
                            );
                        Ok(response)
                    })
                }
            }
        }
    }
    impl<T> Clone for RoutineServiceServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    /// Generated gRPC service name
    pub const SERVICE_NAME: &str = "waltr.routine.v2.RoutineService";
    impl<T> tonic::server::NamedService for RoutineServiceServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
