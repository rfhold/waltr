// @generated
/// Generated client implementations.
pub mod session_service_client {
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
    pub struct SessionServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl<T> SessionServiceClient<T>
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
        ) -> SessionServiceClient<InterceptedService<T, F>>
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
            SessionServiceClient::new(InterceptedService::new(inner, interceptor))
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
        pub async fn create_session(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateSessionResponse>,
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
                "/waltr.session.v1.SessionService/CreateSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "CreateSession"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_sessions(
            &mut self,
            request: impl tonic::IntoRequest<super::ListSessionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListSessionsResponse>,
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
                "/waltr.session.v1.SessionService/ListSessions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "ListSessions"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_session(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DeleteSessionResponse>,
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
                "/waltr.session.v1.SessionService/DeleteSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "DeleteSession"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn wipe_sessions(
            &mut self,
            request: impl tonic::IntoRequest<super::WipeSessionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::WipeSessionsResponse>,
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
                "/waltr.session.v1.SessionService/WipeSessions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "WipeSessions"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_session_runtime_config(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateSessionRuntimeConfigRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UpdateSessionRuntimeConfigResponse>,
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
                "/waltr.session.v1.SessionService/UpdateSessionRuntimeConfig",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.session.v1.SessionService",
                        "UpdateSessionRuntimeConfig",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn watch_sessions(
            &mut self,
            request: impl tonic::IntoRequest<super::WatchSessionsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::WatchSessionsResponse>>,
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
                "/waltr.session.v1.SessionService/WatchSessions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "WatchSessions"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn get_timeline(
            &mut self,
            request: impl tonic::IntoRequest<super::GetTimelineRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetTimelineResponse>,
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
                "/waltr.session.v1.SessionService/GetTimeline",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "GetTimeline"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn watch_timeline(
            &mut self,
            request: impl tonic::IntoRequest<super::WatchTimelineRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::WatchTimelineResponse>>,
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
                "/waltr.session.v1.SessionService/WatchTimeline",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "WatchTimeline"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn submit_user_message(
            &mut self,
            request: impl tonic::IntoRequest<super::SubmitUserMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SubmitUserMessageResponse>,
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
                "/waltr.session.v1.SessionService/SubmitUserMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.session.v1.SessionService",
                        "SubmitUserMessage",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn respond_external_request(
            &mut self,
            request: impl tonic::IntoRequest<super::RespondExternalRequestRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RespondExternalRequestResponse>,
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
                "/waltr.session.v1.SessionService/RespondExternalRequest",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.session.v1.SessionService",
                        "RespondExternalRequest",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn interrupt_session(
            &mut self,
            request: impl tonic::IntoRequest<super::InterruptSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::InterruptSessionResponse>,
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
                "/waltr.session.v1.SessionService/InterruptSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "InterruptSession"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn rollback_session(
            &mut self,
            request: impl tonic::IntoRequest<super::RollbackSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RollbackSessionResponse>,
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
                "/waltr.session.v1.SessionService/RollbackSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "RollbackSession"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn continue_session(
            &mut self,
            request: impl tonic::IntoRequest<super::ContinueSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ContinueSessionResponse>,
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
                "/waltr.session.v1.SessionService/ContinueSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("waltr.session.v1.SessionService", "ContinueSession"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn search_session_templates(
            &mut self,
            request: impl tonic::IntoRequest<super::SearchSessionTemplatesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SearchSessionTemplatesResponse>,
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
                "/waltr.session.v1.SessionService/SearchSessionTemplates",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "waltr.session.v1.SessionService",
                        "SearchSessionTemplates",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod session_service_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with SessionServiceServer.
    #[async_trait]
    pub trait SessionService: std::marker::Send + std::marker::Sync + 'static {
        async fn create_session(
            &self,
            request: tonic::Request<super::CreateSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateSessionResponse>,
            tonic::Status,
        >;
        async fn list_sessions(
            &self,
            request: tonic::Request<super::ListSessionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListSessionsResponse>,
            tonic::Status,
        >;
        async fn delete_session(
            &self,
            request: tonic::Request<super::DeleteSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DeleteSessionResponse>,
            tonic::Status,
        >;
        async fn wipe_sessions(
            &self,
            request: tonic::Request<super::WipeSessionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::WipeSessionsResponse>,
            tonic::Status,
        >;
        async fn update_session_runtime_config(
            &self,
            request: tonic::Request<super::UpdateSessionRuntimeConfigRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UpdateSessionRuntimeConfigResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the WatchSessions method.
        type WatchSessionsStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::WatchSessionsResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn watch_sessions(
            &self,
            request: tonic::Request<super::WatchSessionsRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::WatchSessionsStream>,
            tonic::Status,
        >;
        async fn get_timeline(
            &self,
            request: tonic::Request<super::GetTimelineRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetTimelineResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the WatchTimeline method.
        type WatchTimelineStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::WatchTimelineResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        async fn watch_timeline(
            &self,
            request: tonic::Request<super::WatchTimelineRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::WatchTimelineStream>,
            tonic::Status,
        >;
        async fn submit_user_message(
            &self,
            request: tonic::Request<super::SubmitUserMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SubmitUserMessageResponse>,
            tonic::Status,
        >;
        async fn respond_external_request(
            &self,
            request: tonic::Request<super::RespondExternalRequestRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RespondExternalRequestResponse>,
            tonic::Status,
        >;
        async fn interrupt_session(
            &self,
            request: tonic::Request<super::InterruptSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::InterruptSessionResponse>,
            tonic::Status,
        >;
        async fn rollback_session(
            &self,
            request: tonic::Request<super::RollbackSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RollbackSessionResponse>,
            tonic::Status,
        >;
        async fn continue_session(
            &self,
            request: tonic::Request<super::ContinueSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ContinueSessionResponse>,
            tonic::Status,
        >;
        async fn search_session_templates(
            &self,
            request: tonic::Request<super::SearchSessionTemplatesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SearchSessionTemplatesResponse>,
            tonic::Status,
        >;
    }
    #[derive(Debug)]
    pub struct SessionServiceServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> SessionServiceServer<T> {
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
    impl<T, B> tonic::codegen::Service<http::Request<B>> for SessionServiceServer<T>
    where
        T: SessionService,
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
                "/waltr.session.v1.SessionService/CreateSession" => {
                    #[allow(non_camel_case_types)]
                    struct CreateSessionSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::CreateSessionRequest>
                    for CreateSessionSvc<T> {
                        type Response = super::CreateSessionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateSessionRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::create_session(&inner, request).await
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
                        let method = CreateSessionSvc(inner);
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
                "/waltr.session.v1.SessionService/ListSessions" => {
                    #[allow(non_camel_case_types)]
                    struct ListSessionsSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::ListSessionsRequest>
                    for ListSessionsSvc<T> {
                        type Response = super::ListSessionsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListSessionsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::list_sessions(&inner, request).await
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
                        let method = ListSessionsSvc(inner);
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
                "/waltr.session.v1.SessionService/DeleteSession" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteSessionSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::DeleteSessionRequest>
                    for DeleteSessionSvc<T> {
                        type Response = super::DeleteSessionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DeleteSessionRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::delete_session(&inner, request).await
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
                        let method = DeleteSessionSvc(inner);
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
                "/waltr.session.v1.SessionService/WipeSessions" => {
                    #[allow(non_camel_case_types)]
                    struct WipeSessionsSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::WipeSessionsRequest>
                    for WipeSessionsSvc<T> {
                        type Response = super::WipeSessionsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::WipeSessionsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::wipe_sessions(&inner, request).await
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
                        let method = WipeSessionsSvc(inner);
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
                "/waltr.session.v1.SessionService/UpdateSessionRuntimeConfig" => {
                    #[allow(non_camel_case_types)]
                    struct UpdateSessionRuntimeConfigSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<
                        super::UpdateSessionRuntimeConfigRequest,
                    > for UpdateSessionRuntimeConfigSvc<T> {
                        type Response = super::UpdateSessionRuntimeConfigResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::UpdateSessionRuntimeConfigRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::update_session_runtime_config(
                                        &inner,
                                        request,
                                    )
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
                        let method = UpdateSessionRuntimeConfigSvc(inner);
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
                "/waltr.session.v1.SessionService/WatchSessions" => {
                    #[allow(non_camel_case_types)]
                    struct WatchSessionsSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::ServerStreamingService<super::WatchSessionsRequest>
                    for WatchSessionsSvc<T> {
                        type Response = super::WatchSessionsResponse;
                        type ResponseStream = T::WatchSessionsStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::WatchSessionsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::watch_sessions(&inner, request).await
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
                        let method = WatchSessionsSvc(inner);
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
                "/waltr.session.v1.SessionService/GetTimeline" => {
                    #[allow(non_camel_case_types)]
                    struct GetTimelineSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::GetTimelineRequest>
                    for GetTimelineSvc<T> {
                        type Response = super::GetTimelineResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetTimelineRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::get_timeline(&inner, request).await
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
                        let method = GetTimelineSvc(inner);
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
                "/waltr.session.v1.SessionService/WatchTimeline" => {
                    #[allow(non_camel_case_types)]
                    struct WatchTimelineSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::ServerStreamingService<super::WatchTimelineRequest>
                    for WatchTimelineSvc<T> {
                        type Response = super::WatchTimelineResponse;
                        type ResponseStream = T::WatchTimelineStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::WatchTimelineRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::watch_timeline(&inner, request).await
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
                        let method = WatchTimelineSvc(inner);
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
                "/waltr.session.v1.SessionService/SubmitUserMessage" => {
                    #[allow(non_camel_case_types)]
                    struct SubmitUserMessageSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::SubmitUserMessageRequest>
                    for SubmitUserMessageSvc<T> {
                        type Response = super::SubmitUserMessageResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SubmitUserMessageRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::submit_user_message(&inner, request)
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
                        let method = SubmitUserMessageSvc(inner);
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
                "/waltr.session.v1.SessionService/RespondExternalRequest" => {
                    #[allow(non_camel_case_types)]
                    struct RespondExternalRequestSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::RespondExternalRequestRequest>
                    for RespondExternalRequestSvc<T> {
                        type Response = super::RespondExternalRequestResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RespondExternalRequestRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::respond_external_request(
                                        &inner,
                                        request,
                                    )
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
                        let method = RespondExternalRequestSvc(inner);
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
                "/waltr.session.v1.SessionService/InterruptSession" => {
                    #[allow(non_camel_case_types)]
                    struct InterruptSessionSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::InterruptSessionRequest>
                    for InterruptSessionSvc<T> {
                        type Response = super::InterruptSessionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::InterruptSessionRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::interrupt_session(&inner, request)
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
                        let method = InterruptSessionSvc(inner);
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
                "/waltr.session.v1.SessionService/RollbackSession" => {
                    #[allow(non_camel_case_types)]
                    struct RollbackSessionSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::RollbackSessionRequest>
                    for RollbackSessionSvc<T> {
                        type Response = super::RollbackSessionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RollbackSessionRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::rollback_session(&inner, request)
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
                        let method = RollbackSessionSvc(inner);
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
                "/waltr.session.v1.SessionService/ContinueSession" => {
                    #[allow(non_camel_case_types)]
                    struct ContinueSessionSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::ContinueSessionRequest>
                    for ContinueSessionSvc<T> {
                        type Response = super::ContinueSessionResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ContinueSessionRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::continue_session(&inner, request)
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
                        let method = ContinueSessionSvc(inner);
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
                "/waltr.session.v1.SessionService/SearchSessionTemplates" => {
                    #[allow(non_camel_case_types)]
                    struct SearchSessionTemplatesSvc<T: SessionService>(pub Arc<T>);
                    impl<
                        T: SessionService,
                    > tonic::server::UnaryService<super::SearchSessionTemplatesRequest>
                    for SearchSessionTemplatesSvc<T> {
                        type Response = super::SearchSessionTemplatesResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SearchSessionTemplatesRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as SessionService>::search_session_templates(
                                        &inner,
                                        request,
                                    )
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
                        let method = SearchSessionTemplatesSvc(inner);
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
    impl<T> Clone for SessionServiceServer<T> {
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
    pub const SERVICE_NAME: &str = "waltr.session.v1.SessionService";
    impl<T> tonic::server::NamedService for SessionServiceServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
