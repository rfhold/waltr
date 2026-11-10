//! Authorized discovery, admission, and history methods for the Routine RPC service.
use super::{
    StartPolicy, StartRequest,
    catalog::{Catalog, PublicDefinition},
    cursor::{Codec, Cursor},
    wire,
};
use crate::{
    access_auth::AuthenticatedPrincipal,
    routine::{
        definition::Trigger,
        history::{Caller, Origin, Ownership},
        observation::{Observation, ObservationScope, ObservationStore},
        ports::PortError,
        service::{ChildInvocation, InvocationService},
        text_results::{InvocationTextScope, TextResultError, TextResultStore},
    },
};
use futures_util::{Stream, stream};
use std::{pin::Pin, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use waltr::grpc::waltr::routine::v2 as p;

/// Bounded server stream; dropping it drops its auth waiter and observation state.
pub type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;
/// Shared thin adapter; it owns no executor, provider loop or production route.
#[derive(Clone)]
pub struct ReadAdapter {
    /// The existing invocation owner, including its shared admission gate.
    pub service: InvocationService,
    /// Principal-scoped transactional observation reads.
    pub history: Arc<dyn ObservationStore>,
    /// Complete current per-principal discovery source.
    pub catalog: Arc<dyn Catalog>,
    /// Actual source eligibility and existing host composition.
    pub policy: Arc<dyn StartPolicy>,
    /// Durable selected business text.
    pub text: Arc<dyn TextResultStore>,
}
impl std::fmt::Debug for ReadAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadAdapter").finish_non_exhaustive()
    }
}
pub(super) fn principal<T>(request: &Request<T>) -> Result<AuthenticatedPrincipal, Status> {
    request
        .extensions()
        .get::<AuthenticatedPrincipal>()
        .cloned()
        .ok_or_else(|| Status::unauthenticated("authentication required"))
}
impl ReadAdapter {
    /// Complete discovery snapshots under the caller's current catalog partition.
    pub async fn watch_definitions(
        &self,
        request: Request<p::WatchDefinitionsRequest>,
    ) -> Result<Response<ResponseStream<p::WatchDefinitionsResponse>>, Status> {
        let principal = principal(&request)?;
        let receiver = self
            .catalog
            .subscribe(&super::principal_id(&principal))
            .map_err(wire::error)?;
        let access = super::access_ended(&principal);
        let output = stream::unfold(
            (receiver, access, true, false),
            |(mut receiver, mut access, first, ended)| async move {
                if ended {
                    return None;
                }
                let current = if first {
                    Ok(())
                } else {
                    tokio::select! { ()=&mut access=>Err(Status::unauthenticated("access ended")),changed=receiver.changed()=>changed.map_err(|_|Status::unavailable("catalog closed")) }
                };
                if let Err(error) = current {
                    return Some((Err(error), (receiver, access, false, true)));
                }
                let snapshot = receiver.borrow_and_update().clone();
                let response = (|| {
                    Ok(p::WatchDefinitionsResponse {
                        catalog_revision: snapshot.revision.clone(),
                        definitions: snapshot
                            .entries
                            .values()
                            .map(|entry| {
                                PublicDefinition::capture(entry)
                                    .map_err(wire::error)
                                    .and_then(wire::definition)
                            })
                            .collect::<Result<_, Status>>()?,
                        supported_capabilities: snapshot
                            .supported_capabilities
                            .iter()
                            .cloned()
                            .collect(),
                        limits: Some(limits()),
                    })
                })();
                let failed = response.is_err();
                Some((response, (receiver, access, false, failed)))
            },
        );
        Ok(Response::new(Box::pin(output)))
    }
    /// Detached manual admission, preserving the exact historical public offers.
    pub async fn start_invocation(
        &self,
        request: Request<p::StartInvocationRequest>,
    ) -> Result<Response<p::StartInvocationResponse>, Status> {
        use tracing::Instrument as _;
        let operation = crate::routine::telemetry::rpc(request.metadata(), true);
        let result = self
            .start_authorized_invocation(request)
            .instrument(operation.span.clone())
            .await;
        operation.span.record(
            "grpc_status",
            result
                .as_ref()
                .err()
                .map_or(0, |status| status.code() as i32),
        );
        operation.outcome(if result.is_ok() { "success" } else { "failure" });
        result
    }
    async fn start_authorized_invocation(
        &self,
        request: Request<p::StartInvocationRequest>,
    ) -> Result<Response<p::StartInvocationResponse>, Status> {
        let principal = principal(&request)?;
        let owner = super::principal_id(&principal);
        let selection = request
            .into_inner()
            .selection
            .ok_or_else(|| Status::invalid_argument("missing selection"))?;
        let (entry, input) = crate::routine::telemetry::admission_sync(
            crate::routine::telemetry::AdmissionStage::Prepare,
            || self.selection(&owner, &selection, Trigger::Manual),
        )?;
        let source = selection.source_session.clone();
        let authorized = self
            .policy
            .authorize(StartRequest {
                authenticated: principal.clone(),
                transport: None,
                principal: owner.clone(),
                definition: entry.definition.clone(),
                entrypoint: selection.entrypoint.clone(),
                source_session: source.clone(),
            })
            .await
            .map_err(wire::error)?;
        if authorized.source_session != source {
            return Err(Status::permission_denied("source binding mismatch"));
        }
        let projection =
            serde_json::to_value(PublicDefinition::capture(&entry).map_err(wire::error)?)
                .map_err(|_| Status::internal("public projection"))?;
        let admission = super::invoke_authorized(
            &self.service,
            ChildInvocation {
                definition: entry.definition,
                entrypoint: selection.entrypoint,
                caller: Caller {
                    principal: owner.clone(),
                    origin: Origin::Manual,
                    ownership: Ownership::Detached,
                    correlation: selection.correlation,
                },
                input,
            },
            authorized,
            projection,
            None,
        )
        .map_err(wire::error)?;
        let id = admission.owner.id.clone();
        let _admitted = admission.admitted.await.map_err(wire::error)?;
        let view = self
            .history
            .get(
                ObservationScope {
                    principal: owner,
                    source_session: None,
                },
                id,
            )
            .await
            .map_err(wire::error)?
            .ok_or_else(|| Status::unavailable("admission view unavailable"))?;
        Ok(Response::new(p::StartInvocationResponse {
            invocation: Some(wire::invocation(view)?),
        }))
    }
    pub(super) fn selection(
        &self,
        principal: &str,
        selection: &p::InvocationSelection,
        trigger: Trigger,
    ) -> Result<(super::catalog::CatalogEntry, serde_json::Value), Status> {
        wire::id(&selection.routine, 128)?;
        wire::id(&selection.revision, 256)?;
        wire::id(&selection.entrypoint, 128)?;
        if selection.correlation.len() > 128 {
            return Err(Status::invalid_argument("invalid correlation"));
        }
        if let Some(source) = &selection.source_session {
            wire::id(source, 256)?;
        }
        let catalog = self.catalog.subscribe(principal).map_err(wire::error)?;
        let snapshot = catalog.borrow();
        let entry = snapshot
            .entries
            .get(&selection.routine)
            .ok_or_else(|| Status::permission_denied("routine unavailable"))?;
        if entry.definition.revision() != selection.revision {
            return Err(Status::failed_precondition("stale selection"));
        }
        if !entry.permits(&selection.entrypoint, selection.source_session.is_some()) {
            return Err(Status::permission_denied("start context unavailable"));
        }
        let descriptor = entry
            .definition
            .envelope()
            .spec
            .entrypoints
            .get(&selection.entrypoint)
            .ok_or_else(|| Status::invalid_argument("unknown entrypoint"))?;
        if wire::trigger(descriptor.trigger) != wire::trigger(trigger) {
            return Err(Status::invalid_argument("wrong entrypoint family"));
        }
        let input = wire::structured(selection.parameters.clone())?;
        entry
            .definition
            .envelope()
            .spec
            .parameters
            .accepts(&input, entry.definition.limits())
            .map_err(|_| Status::invalid_argument("parameter contract"))?;
        Ok((entry.clone(), input))
    }
    pub(super) async fn scope(
        &self,
        principal: &AuthenticatedPrincipal,
        source: Option<String>,
    ) -> Result<ObservationScope, Status> {
        if let Some(source) = &source {
            wire::id(source, 256)?;
        }
        let principal = super::principal_id(principal);
        self.policy
            .source_access(principal.clone(), source.clone())
            .await
            .map_err(wire::error)?;
        Ok(ObservationScope {
            principal,
            source_session: source,
        })
    }
    /// Authorized admission-order pagination, filtered before SQL LIMIT.
    pub async fn list_invocations(
        &self,
        request: Request<p::ListInvocationsRequest>,
    ) -> Result<Response<p::ListInvocationsResponse>, Status> {
        let principal = principal(&request)?;
        let request = request.into_inner();
        let scope = self.scope(&principal, request.source_session).await?;
        let after = wire::cursor(request.after_cursor)?;
        let invocations = self
            .history
            .list(scope, after, request.limit)
            .await
            .map_err(wire::error)?
            .into_iter()
            .map(wire::invocation)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = invocations
            .last()
            .map_or(request.after_cursor, |row| row.cursor);
        Ok(Response::new(p::ListInvocationsResponse {
            invocations,
            next_cursor,
        }))
    }
    /// One atomic versioned record and its exact admission projection.
    pub async fn get_invocation(
        &self,
        request: Request<p::GetInvocationRequest>,
    ) -> Result<Response<p::GetInvocationResponse>, Status> {
        let principal = principal(&request)?;
        let id = request.into_inner().invocation_id;
        let view = self.authorize(&principal, &id).await?;
        let definition = wire::definition(wire::pinned(&view)?)?;
        Ok(Response::new(p::GetInvocationResponse {
            invocation: Some(wire::invocation(view)?),
            pinned_definition: Some(definition),
        }))
    }
    pub(super) async fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &str,
    ) -> Result<crate::routine::observation::InvocationView, Status> {
        wire::id(id, 256)?;
        self.history
            .get(
                ObservationScope {
                    principal: super::principal_id(principal),
                    source_session: None,
                },
                id.into(),
            )
            .await
            .map_err(wire::error)?
            .ok_or_else(|| Status::permission_denied("invocation unavailable"))
    }
    /// Durable fixed-high-water snapshots and ordered changes; never execution replay.
    pub async fn watch_invocations(
        &self,
        request: Request<p::WatchInvocationsRequest>,
    ) -> Result<Response<ResponseStream<p::WatchInvocationsResponse>>, Status> {
        let principal = principal(&request)?;
        let request = request.into_inner();
        if !(1..=100).contains(&request.limit) {
            return Err(Status::invalid_argument("invalid page limit"));
        }
        let scope = self.scope(&principal, request.source_session).await?;
        let codec = Codec::new(self.history.cursor_key().await.map_err(wire::error)?);
        let cursor = if request.after_cursor.is_empty() {
            None
        } else {
            Some(codec.decode(&scope, &request.after_cursor)?)
        };
        let state = (
            self.history.clone(),
            scope,
            codec,
            cursor,
            super::access_ended(&principal),
            false,
            request.limit,
        );
        let output = stream::unfold(
            state,
            |(store, scope, codec, mut cursor, mut access, ended, limit)| async move {
                if ended {
                    return None;
                }
                loop {
                    let current = cursor.unwrap_or(Cursor {
                        high: 0,
                        position: 0,
                        snapshot: true,
                    });
                    let page = tokio::select! { ()=&mut access=>Err(Status::unauthenticated("access ended")),page=store.page(scope.clone(),cursor.map(|c|c.high),current.position,current.snapshot,limit)=>page.map_err(|error|if error==PortError::Conflict { Status::out_of_range("observation cursor unavailable; resnapshot required") } else { wire::error(error) }) };
                    let page = match page {
                        Ok(page) => page,
                        Err(error) => {
                            return Some((
                                Err(error),
                                (store, scope, codec, cursor, access, true, limit),
                            ));
                        }
                    };
                    if !current.snapshot && page.rows.is_empty() {
                        tokio::select! { ()=&mut access=>return Some((Err(Status::unauthenticated("access ended")),(store,scope,codec,cursor,access,true,limit))),()=tokio::time::sleep(Duration::from_millis(250))=>{} }
                        continue;
                    }
                    let position = Cursor {
                        high: page.high_water,
                        position: page.position,
                        snapshot: current.snapshot && !page.complete,
                    };
                    let high = codec.encode(
                        &scope,
                        Cursor {
                            high: page.high_water,
                            position: page.high_water,
                            snapshot: false,
                        },
                    );
                    let token = if current.snapshot && page.complete {
                        high.clone()
                    } else {
                        codec.encode(&scope, position)
                    };
                    let result = (|| {
                        let mut response = p::WatchInvocationsResponse {
                            cursor: token,
                            snapshot_complete: !current.snapshot || page.complete,
                            phase: if current.snapshot { 1 } else { 2 },
                            snapshot_high_water: high,
                            ..Default::default()
                        };
                        for row in page.rows {
                            match row {
                                Observation::Invocation(row) => {
                                    response.invocations.push(wire::invocation(row)?)
                                }
                                Observation::Operation(row) => {
                                    response.operations.push(wire::operation(row)?)
                                }
                            }
                        }
                        Ok(response)
                    })();
                    cursor = Some(position);
                    let failed = result.is_err();
                    return Some((result, (store, scope, codec, cursor, access, failed, limit)));
                }
            },
        );
        Ok(Response::new(Box::pin(output)))
    }
    /// Authorized immutable operation-order pagination with current mutation revisions.
    pub async fn list_operations(
        &self,
        request: Request<p::ListOperationsRequest>,
    ) -> Result<Response<p::ListOperationsResponse>, Status> {
        let principal = principal(&request)?;
        let request = request.into_inner();
        wire::id(&request.invocation_id, 256)?;
        let operations = self
            .history
            .operations(
                ObservationScope {
                    principal: super::principal_id(&principal),
                    source_session: None,
                },
                request.invocation_id,
                wire::cursor(request.after_cursor)?,
                request.limit,
            )
            .await
            .map_err(wire::error)?
            .into_iter()
            .map(wire::operation)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = operations
            .last()
            .map_or(request.after_cursor, |row| row.cursor);
        Ok(Response::new(p::ListOperationsResponse {
            operations,
            next_cursor,
        }))
    }
    /// Idempotent authorized cancellation, independent of data queue capacity.
    pub async fn cancel_invocation(
        &self,
        request: Request<p::CancelInvocationRequest>,
    ) -> Result<Response<p::CancelInvocationResponse>, Status> {
        let principal = principal(&request)?;
        let request = request.into_inner();
        let view = self.authorize(&principal, &request.invocation_id).await?;
        if matches!(
            view.invocation.status,
            crate::routine::history::Status::Admitted | crate::routine::history::Status::Running
        ) {
            self.service
                .cancel(&request.invocation_id)
                .map_err(wire::error)?;
        }
        Ok(Response::new(p::CancelInvocationResponse {
            status: wire::invocation(view)?.status,
        }))
    }
    /// Durable selected text read with explicit raw-byte coordinates.
    pub async fn read_text_result(
        &self,
        request: Request<p::ReadTextResultRequest>,
    ) -> Result<Response<p::ReadTextResultResponse>, Status> {
        let principal = principal(&request)?;
        let request = request.into_inner();
        wire::id(&request.result_id, 256)?;
        let view = self.authorize(&principal, &request.invocation_id).await?;
        let descriptor = view
            .text_results
            .iter()
            .find(|row| row.id == request.result_id)
            .ok_or_else(|| Status::permission_denied("result unavailable"))?;
        let scope =
            InvocationTextScope::new(super::principal_id(&principal), request.invocation_id)
                .map_err(text_error)?;
        let chunk = self
            .text
            .read(
                scope,
                request.result_id,
                request.offset,
                request.max_bytes,
                CancellationToken::new(),
            )
            .await
            .map_err(text_error)?;
        let next_offset = request.offset + chunk.bytes().len() as u64;
        Ok(Response::new(p::ReadTextResultResponse {
            data: chunk.bytes().to_vec(),
            next_offset,
            eof: next_offset == descriptor.utf8_bytes,
        }))
    }
}
fn text_error(error: TextResultError) -> Status {
    match error {
        TextResultError::Denied => Status::permission_denied("result unavailable"),
        TextResultError::Invalid => Status::invalid_argument("invalid text read"),
        TextResultError::Conflict => Status::failed_precondition("result state changed"),
        TextResultError::Quota => Status::resource_exhausted("text quota"),
        TextResultError::Cancelled => Status::cancelled("text read cancelled"),
        TextResultError::Unavailable | TextResultError::Uncertain => {
            Status::unavailable("text service unavailable")
        }
    }
}
/// Foundation ceilings, narrowed during admission, except legacy image placeholders.
pub fn limits() -> p::ServerLimits {
    p::ServerLimits {
        chunk_bytes: 65536,
        input_batch_bytes: 131072,
        input_batch_events: 32,
        edge_bytes: 1048576,
        edge_chunks: 16,
        input_bytes: 268435456,
        output_bytes: 268435456,
        input_duration_ms: 3900000,
        total_duration_ms: 5400000,
        queue_wait_ms: 30000,
        idle_ms: 30000,
        streams_per_direction: 4,
        text_segments: 4096,
        retained_text_bytes: 2097152,
        attachment_lease_ms: 30000,
        attachment_attach_timeout_ms: 5000,
        attachment_absolute_lifetime_ms: 5400000,
        file_bytes: 67108864,
        image_bytes: 20971520,
        // Nonzero compatibility placeholders for current consumers, not enforced
        // caps or trusted image facts; must not govern client security acceptance.
        // Images receive format/MIME/minimum signature checks, not local decoding.
        image_dimension: 8192,
        image_pixels: 32000000,
        helper_attachment_bytes: 8388608,
        helper_source_bytes: 16777216,
        helper_encoded_bytes: 33554432,
        helper_attachments: 4,
        binary_output_lease_ms: 900000,
        text_result_bytes: 1048576,
        text_results_encoded_bytes: 8388608,
        text_results: 32,
        page_size: 100,
        structured_value_bytes: 65536,
        definition_bytes: 65536,
        definitions_per_snapshot: 256,
        definition_snapshot_bytes: 2097152,
        operation_projection_bytes: 4096,
        blob_invocation_bytes: 536870912,
        blob_principal_bytes: 1073741824,
        blob_objects: 128,
        blob_readers: 16,
        ordinary_deadline_ms: 30000,
    }
}
