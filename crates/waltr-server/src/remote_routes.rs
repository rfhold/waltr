use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::http::{HeaderMap, Request, Response};
use futures_util::StreamExt;
use http_body::{Body as _, Frame};
use prost::Message;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tonic::{
    Status, Streaming,
    body::Body,
    codegen::BoxStream,
    metadata::AsciiMetadataValue,
    server::{Grpc, ServerStreamingService, StreamingService, UnaryService},
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use waltr::{
    destination,
    grpc::waltr::{
        auth::v1 as auth, catalog::v1 as catalog, resource::v1 as resource, routine::v2 as routine,
        session::v1 as session, status::v1 as status,
    },
};

use crate::{
    agent_definition::AgentDefinitionState,
    auth::{AuthService, RemoteCredentialLease, now_ms, remote_route_id},
    resource_catalog::PublishedState,
};

#[derive(Clone, Debug)]
pub(crate) struct RemoteRoutes {
    catalog: AgentDefinitionState,
    auth: Arc<AuthService>,
}

impl RemoteRoutes {
    pub(crate) const fn new(catalog: AgentDefinitionState, auth: Arc<AuthService>) -> Self {
        Self { catalog, auth }
    }

    pub(crate) async fn forward(
        self,
        selector: String,
        request: Request<Body>,
        entry_lifetime: Option<Arc<CancellationToken>>,
    ) -> Response<Body> {
        match self.route(&selector, request, entry_lifetime).await {
            Ok(response) => response,
            Err(status) => status.into_http(),
        }
    }

    async fn route(
        &self,
        selector: &str,
        request: Request<Body>,
        entry_lifetime: Option<Arc<CancellationToken>>,
    ) -> Result<Response<Body>, Status> {
        if !supported(request.uri().path()) {
            return Err(Status::unimplemented(
                "RPC does not support a Remote destination",
            ));
        }
        let (name, expected) = destination::pinned(selector).ok_or_else(|| {
            Status::failed_precondition("Resolve the Remote authorization before routing")
        })?;
        let publications = self.catalog.subscribe();
        let snapshot = self.catalog.snapshot();
        let remote = snapshot
            .remotes()
            .get(name)
            .ok_or_else(|| Status::not_found("Remote destination not found"))?;
        let endpoint = remote.endpoint().clone();
        let incarnation = snapshot.remote_incarnation(name);
        drop(snapshot);

        let (lease, lifetime) = self.auth.remote_route_lease(name, &endpoint, expected).await?;
        if remote_route_id(&lease.authority_id).as_deref() != Some(expected) {
            return Err(Status::failed_precondition(
                "Remote authorization changed; reopen the destination",
            ));
        }
        let mut guard = RouteGuard {
            catalog: self.catalog.clone(),
            name: name.to_owned(),
            incarnation,
            publications,
            lifetime,
            entry_lifetime,
            expiry: tokio::time::Instant::now()
                + Duration::from_millis(
                    u64::try_from(lease.expires_at_ms.saturating_sub(now_ms())).unwrap_or(0),
                ),
        };
        let mut endpoint_builder = Endpoint::from_shared(endpoint.to_string())
            .map_err(|_| Status::internal("Remote endpoint is invalid"))?
            .connect_timeout(Duration::from_secs(5));
        if endpoint.scheme() == "https" {
            endpoint_builder = endpoint_builder
                .tls_config(ClientTlsConfig::new().with_webpki_roots())
                .map_err(|_| Status::internal("Remote TLS could not be configured"))?;
        }
        let outbound = Outbound::new(endpoint_builder.connect_lazy(), lease, request.headers())?;
        let response = tokio::select! {
            biased;
            status = guard.ended() => return Err(status),
            response = dispatch(outbound, request) => response,
        };
        if response.body().is_end_stream() {
            return Ok(response);
        }
        let (parts, body) = response.into_parts();
        Ok(Response::from_parts(
            parts,
            Body::new(GuardedBody {
                source: Some(body),
                ended: Box::pin(async move { guard.ended().await }),
            }),
        ))
    }
}

pub(crate) fn take_destination(headers: &mut HeaderMap) -> Result<Option<String>, Status> {
    let mut values = headers.get_all(destination::HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| Status::invalid_argument("invalid destination"))?;
    let name = match (value, values.next()) {
        (destination::LOCAL, None) => None,
        (selector, None)
            if destination::pinned(selector).is_some() || destination::is_remote_name(selector) =>
        {
            Some(selector.to_owned())
        }
        _ => return Err(Status::invalid_argument("invalid destination")),
    };
    headers.remove(destination::HEADER);
    Ok(name)
}

struct RouteGuard {
    catalog: AgentDefinitionState,
    name: String,
    incarnation: Option<uuid::Uuid>,
    publications: broadcast::Receiver<PublishedState>,
    lifetime: Arc<CancellationToken>,
    entry_lifetime: Option<Arc<CancellationToken>>,
    expiry: tokio::time::Instant,
}

impl RouteGuard {
    async fn ended(&mut self) -> Status {
        let changed = async {
            loop {
                let snapshot = self.catalog.snapshot();
                if !snapshot.remotes().contains_key(&self.name)
                    || snapshot.remote_incarnation(&self.name) != self.incarnation
                {
                    break;
                }
                if self.publications.recv().await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            biased;
            () = async {
                match &self.entry_lifetime {
                    Some(lifetime) => lifetime.cancelled().await,
                    None => std::future::pending().await,
                }
            } => Status::cancelled("entry server stopped"),
            () = self.lifetime.cancelled() => Status::failed_precondition("Remote authorization ended"),
            () = tokio::time::sleep_until(self.expiry) => Status::failed_precondition("Remote authorization expired"),
            () = changed => Status::aborted("Remote destination changed"),
        }
    }
}

struct GuardedBody {
    source: Option<Body>,
    ended: Pin<Box<dyn Future<Output = Status> + Send>>,
}

impl http_body::Body for GuardedBody {
    type Data = axum::body::Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Status>>> {
        if self.source.is_none() {
            return Poll::Ready(None);
        }
        if let Poll::Ready(status) = self.ended.as_mut().poll(cx) {
            self.source = None;
            let mut headers = status.into_http::<()>().into_parts().0.headers;
            headers.remove(axum::http::header::CONTENT_TYPE);
            return Poll::Ready(Some(Ok(Frame::trailers(headers))));
        }
        Pin::new(self.source.as_mut().expect("checked source")).poll_frame(cx)
    }
}

#[derive(Clone)]
struct Outbound {
    channel: Channel,
    authorization: AsciiMetadataValue,
    grpc_timeout: Option<AsciiMetadataValue>,
}

impl Outbound {
    fn new(
        channel: Channel,
        lease: RemoteCredentialLease,
        headers: &HeaderMap,
    ) -> Result<Self, Status> {
        let mut authorization: AsciiMetadataValue = format!("Bearer {}", lease.access_token)
            .parse()
            .map_err(|_| Status::internal("invalid Remote credential"))?;
        authorization.set_sensitive(true);
        let grpc_timeout = headers
            .get("grpc-timeout")
            .and_then(|header| AsciiMetadataValue::try_from(header.as_bytes()).ok());
        Ok(Self {
            channel,
            authorization,
            grpc_timeout,
        })
    }

    fn request<T>(&self, body: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(body);
        request
            .metadata_mut()
            .insert("authorization", self.authorization.clone());
        if let Some(timeout) = &self.grpc_timeout {
            request
                .metadata_mut()
                .insert("grpc-timeout", timeout.clone());
        }
        request
    }
}

fn map_status(status: Status) -> Status {
    // Neither a downstream access error nor its challenge is evidence about entry login.
    if auth::AuthenticationFailure::decode(status.details())
        .is_ok_and(|failure| failure.domain == auth::AuthenticationDomain::WaltrAccess as i32)
    {
        return Status::failed_precondition("Remote access rejected");
    }
    if status.code() == tonic::Code::Unauthenticated && status.details().is_empty() {
        return Status::failed_precondition("Remote access rejected");
    }
    Status::with_details(
        status.code(),
        status.message(),
        status.details().to_vec().into(),
    )
}

struct Call<F>(F);

impl<I, O, F, Fut> UnaryService<I> for Call<F>
where
    F: FnMut(tonic::Request<I>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<O>, Status>>,
{
    type Response = O;
    type Future = Fut;
    fn call(&mut self, request: tonic::Request<I>) -> Self::Future {
        (self.0)(request)
    }
}

impl<I, O, F, Fut> ServerStreamingService<I> for Call<F>
where
    F: FnMut(tonic::Request<I>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<OutputStream<O>>, Status>>,
{
    type Response = O;
    type ResponseStream = OutputStream<O>;
    type Future = Fut;
    fn call(&mut self, request: tonic::Request<I>) -> Self::Future {
        (self.0)(request)
    }
}

impl<I, O, F, Fut> StreamingService<I> for Call<F>
where
    F: FnMut(tonic::Request<Streaming<I>>) -> Fut,
    Fut: Future<Output = Result<tonic::Response<OutputStream<O>>, Status>>,
{
    type Response = O;
    type ResponseStream = OutputStream<O>;
    type Future = Fut;
    fn call(&mut self, request: tonic::Request<Streaming<I>>) -> Self::Future {
        (self.0)(request)
    }
}

struct OutputStream<T>(BoxStream<T>);

impl<T> futures_util::Stream for OutputStream<T> {
    type Item = Result<T, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.as_mut().poll_next(cx)
    }
}

fn stream_response<T: prost::Message + Default + Send + 'static>(
    response: tonic::Response<Streaming<T>>,
) -> tonic::Response<OutputStream<T>> {
    tonic::Response::new(OutputStream(Box::pin(
        response.into_inner().map(|item| item.map_err(map_status)),
    )))
}

type StreamCall<T> =
    Pin<Box<dyn Future<Output = Result<tonic::Response<OutputStream<T>>, Status>> + Send>>;

fn codec<I: Message + Default + Send + 'static, O: Message + Send + 'static>(
    service: &str,
) -> Grpc<tonic_prost::ProstCodec<O, I>> {
    let grpc = Grpc::new(tonic_prost::ProstCodec::default());
    if service == "waltr.routine.v2.RoutineService" {
        grpc.max_decoding_message_size(131_072)
            .max_encoding_message_size(16_777_216)
    } else {
        grpc
    }
}

fn entry_control_plane(path: &str) -> Result<(), Status> {
    if path.split('/').next() == Some("remotes") {
        return Err(Status::failed_precondition(
            "Remote administration belongs to the entry server",
        ));
    }
    Ok(())
}

fn auth_control_plane(target: &Option<auth::AuthTarget>) -> Result<(), Status> {
    if matches!(
        target.as_ref().and_then(|target| target.target.as_ref()),
        Some(auth::auth_target::Target::RemoteKey(_))
    ) {
        return entry_control_plane("remotes");
    }
    Ok(())
}

fn attempt_control_plane(id: &str) -> Result<(), Status> {
    if id.starts_with("remote-") {
        return entry_control_plane("remotes");
    }
    Ok(())
}

macro_rules! check_unary {
    (replace_resource, $request:ident) => {
        entry_control_plane(&$request.path)
    };
    (reset_resource, $request:ident) => {
        entry_control_plane(&$request.path)
    };
    (get_auth_status, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (begin_auth, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (refresh_auth, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (logout, $request:ident) => {
        auth_control_plane(&$request.target)
    };
    (complete_auth, $request:ident) => {
        attempt_control_plane(&$request.attempt_id)
    };
    (get_auth_attempt_status, $request:ident) => {
        attempt_control_plane(&$request.attempt_id)
    };
    (cancel_auth, $request:ident) => {
        attempt_control_plane(&$request.attempt_id)
    };
    ($method:ident, $request:ident) => {
        Ok::<(), Status>(())
    };
}

macro_rules! forward_call {
    (unary, $out:ident, $req:ident, $service:literal, $client:path, $input:path, $method:ident) => {
        codec($service).unary(Call(move |request: tonic::Request<$input>| {
            let outbound = $out.clone();
            async move {
                let request = request.into_inner();
                check_unary!($method, request)?;
                let mut client = <$client>::new(outbound.channel.clone());
                let response = client.$method(outbound.request(request)).await.map_err(map_status)?;
                Ok(tonic::Response::new(response.into_inner()))
            }
        }), $req).await
    };
    (stream, $out:ident, $req:ident, $service:literal, $client:path, $input:path, $method:ident) => {
        codec($service).server_streaming(Call(move |request: tonic::Request<$input>| -> StreamCall<_> {
            let outbound = $out.clone();
            Box::pin(async move {
                let mut client = <$client>::new(outbound.channel.clone()).max_decoding_message_size(16_777_216);
                let response = client.$method(outbound.request(request.into_inner())).await.map_err(map_status)?;
                Ok(stream_response(response))
            })
        }), $req).await
    };
    (duplex, $out:ident, $req:ident, $service:literal, $client:path, $input:path, $method:ident) => {
        codec($service).streaming(Call(move |request: tonic::Request<Streaming<$input>>| -> StreamCall<_> {
            let outbound = $out.clone();
            Box::pin(async move {
                let mut client = <$client>::new(outbound.channel.clone()).max_decoding_message_size(16_777_216);
                let failed = CancellationToken::new();
                let input = request.into_inner().scan(failed.clone(), |failed, item| {
                    let value = item.map_err(|_| failed.cancel()).ok();
                    std::future::ready(value)
                });
                let response = tokio::select! {
                    biased;
                    () = failed.cancelled() => return Err(Status::invalid_argument("invalid Remote stream input")),
                    response = client.$method(outbound.request(input)) => response.map_err(map_status)?,
                };
                let stream = futures_util::stream::unfold(Some((response.into_inner(), failed)), |state| async {
                    let (mut output, failed) = state?;
                    match tokio::select! {
                            biased;
                            () = failed.cancelled() => Err(Status::invalid_argument("invalid Remote stream input")),
                            item = output.message() => item.map_err(map_status),
                    } {
                        Ok(Some(item)) => Some((Ok(item), Some((output, failed)))),
                        Ok(None) => None,
                        Err(error) => Some((Err(error), None)),
                    }
                });
                Ok(tonic::Response::new(OutputStream(Box::pin(stream))))
            })
        }), $req).await
    };
}

macro_rules! routes {
    ($($service:literal => $client:path {
        $($shape:ident $rpc:literal => $method:ident($input:path);)*
    })*) => {
        fn supported(path: &str) -> bool {
            matches!(path, $($(concat!("/", $service, "/", $rpc))|*)|*)
        }

        async fn dispatch(outbound: Outbound, request: Request<Body>) -> Response<Body> {
            match request.uri().path() {
                $($(concat!("/", $service, "/", $rpc) =>
                    forward_call!($shape, outbound, request, $service, $client, $input, $method),)*)*
                _ => Status::unimplemented("RPC does not support a Remote destination").into_http(),
            }
        }
    };
}

routes! {
    "waltr.session.v1.SessionService" => session::session_service_client::SessionServiceClient<Channel> {
        unary "OpenSessionMcpApp" => open_session_mcp_app(session::OpenSessionMcpAppRequest);
        unary "CallSessionMcpAppTool" => call_session_mcp_app_tool(session::CallSessionMcpAppToolRequest);
        unary "CloseSessionMcpApp" => close_session_mcp_app(session::CloseSessionMcpAppRequest);
        unary "ListAgents" => list_agents(session::ListAgentsRequest);
        unary "CreateSession" => create_session(session::CreateSessionRequest);
        unary "ListSessions" => list_sessions(session::ListSessionsRequest);
        unary "DeleteSession" => delete_session(session::DeleteSessionRequest);
        unary "WipeSessions" => wipe_sessions(session::WipeSessionsRequest);
        unary "UpdateSessionInferenceSettings" => update_session_inference_settings(session::UpdateSessionInferenceSettingsRequest);
        unary "UpdateSessionAgent" => update_session_agent(session::UpdateSessionAgentRequest);
        stream "WatchSessions" => watch_sessions(session::WatchSessionsRequest);
        unary "GetTimeline" => get_timeline(session::GetTimelineRequest);
        stream "WatchTimeline" => watch_timeline(session::WatchTimelineRequest);
        unary "SubmitUserMessage" => submit_user_message(session::SubmitUserMessageRequest);
        unary "InterruptSession" => interrupt_session(session::InterruptSessionRequest);
        unary "RollbackSession" => rollback_session(session::RollbackSessionRequest);
        unary "ContinueSession" => continue_session(session::ContinueSessionRequest);
        unary "SearchSessionTemplates" => search_session_templates(session::SearchSessionTemplatesRequest);
    }
    "waltr.session.v1.InteractionService" => session::interaction_service_client::InteractionServiceClient<Channel> {
        unary "GetInteractions" => get_interactions(session::GetInteractionsRequest);
        stream "WatchInteractions" => watch_interactions(session::WatchInteractionsRequest);
        unary "RespondInteraction" => respond_interaction(session::RespondInteractionRequest);
    }
    "waltr.routine.v2.RoutineService" => routine::routine_service_client::RoutineServiceClient<Channel> {
        stream "WatchDefinitions" => watch_definitions(routine::WatchDefinitionsRequest);
        unary "StartInvocation" => start_invocation(routine::StartInvocationRequest);
        unary "ListInvocations" => list_invocations(routine::ListInvocationsRequest);
        unary "GetInvocation" => get_invocation(routine::GetInvocationRequest);
        stream "WatchInvocations" => watch_invocations(routine::WatchInvocationsRequest);
        unary "ListOperations" => list_operations(routine::ListOperationsRequest);
        unary "CancelInvocation" => cancel_invocation(routine::CancelInvocationRequest);
        unary "ReadTextResult" => read_text_result(routine::ReadTextResultRequest);
        duplex "Interact" => interact(routine::InteractRequest);
        unary "OpenAttachment" => open_attachment(routine::OpenAttachmentRequest);
        unary "SendInputBatch" => send_input_batch(routine::SendInputBatchRequest);
        stream "WatchLiveOutput" => watch_live_output(routine::WatchLiveOutputRequest);
        unary "RenewAttachment" => renew_attachment(routine::RenewAttachmentRequest);
        unary "CloseAttachment" => close_attachment(routine::CloseAttachmentRequest);
        unary "BeginBlobUpload" => begin_blob_upload(routine::BeginBlobUploadRequest);
        unary "AppendBlobUpload" => append_blob_upload(routine::AppendBlobUploadRequest);
        unary "CommitBlobUpload" => commit_blob_upload(routine::CommitBlobUploadRequest);
        unary "CancelBlobUpload" => cancel_blob_upload(routine::CancelBlobUploadRequest);
        unary "OpenBlobRead" => open_blob_read(routine::OpenBlobReadRequest);
        stream "ReadBlob" => read_blob(routine::ReadBlobRequest);
        unary "CloseBlobRead" => close_blob_read(routine::CloseBlobReadRequest);
    }
    "waltr.resource.v1.ResourceService" => resource::resource_service_client::ResourceServiceClient<Channel> {
        unary "GetResourceCatalog" => get_resource_catalog(resource::GetResourceCatalogRequest);
        stream "WatchResourceCatalog" => watch_resource_catalog(resource::WatchResourceCatalogRequest);
        unary "InspectResource" => inspect_resource(resource::InspectResourceRequest);
        unary "ReplaceResource" => replace_resource(resource::ReplaceResourceRequest);
        unary "ResetResource" => reset_resource(resource::ResetResourceRequest);
    }
    "waltr.catalog.v1.CatalogService" => catalog::catalog_service_client::CatalogServiceClient<Channel> {
        unary "SearchCatalogModels" => search_catalog_models(catalog::SearchCatalogModelsRequest);
        unary "SuggestCatalogMatches" => suggest_catalog_matches(catalog::SuggestCatalogMatchesRequest);
        unary "GetModelCatalogDrift" => get_model_catalog_drift(catalog::GetModelCatalogDriftRequest);
    }
    "waltr.auth.v1.AuthService" => auth::auth_service_client::AuthServiceClient<Channel> {
        unary "ListAuthStatuses" => list_auth_statuses(auth::ListAuthStatusesRequest);
        unary "GetAuthStatus" => get_auth_status(auth::GetAuthStatusRequest);
        unary "BeginAuth" => begin_auth(auth::BeginAuthRequest);
        unary "CompleteAuth" => complete_auth(auth::CompleteAuthRequest);
        unary "GetAuthAttemptStatus" => get_auth_attempt_status(auth::GetAuthAttemptStatusRequest);
        unary "CancelAuth" => cancel_auth(auth::CancelAuthRequest);
        unary "RefreshAuth" => refresh_auth(auth::RefreshAuthRequest);
        unary "Logout" => logout(auth::LogoutRequest);
    }
    "waltr.status.v1.StatusService" => status::status_service_client::StatusServiceClient<Channel> {
        stream "WatchStatus" => watch_status(status::WatchStatusRequest);
    }
}
