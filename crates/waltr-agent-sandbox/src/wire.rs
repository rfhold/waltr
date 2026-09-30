//! Private worker-to-host streaming RPC. The host never executes tools locally.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use waltr::grpc::waltr::sandbox::v1::{
    self as proto, connect_request::Body as WorkerBody, connect_response::Body as HostBody,
    sandbox_service_client::SandboxServiceClient,
};
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;

use crate::{Sandbox, SandboxError, tools::ShellDefaults};

fn shell_defaults(request: &proto::Prepare) -> Result<Option<ShellDefaults>, SandboxError> {
    if request.tool_name != "shell" {
        if request.shell_defaults.is_some() {
            return Err(SandboxError("shell defaults on non-shell tool".into()));
        }
        return Ok(None);
    }
    let defaults = request
        .shell_defaults
        .as_ref()
        .ok_or_else(|| SandboxError("shell defaults required".into()))?;
    if defaults.default_timeout_nanos >= 1_000_000_000 {
        return Err(SandboxError("invalid shell timeout nanos".into()));
    }
    Ok(Some(ShellDefaults {
        default_workdir: PathBuf::from(&defaults.default_workdir),
        default_timeout: Duration::new(
            defaults.default_timeout_seconds,
            defaults.default_timeout_nanos,
        ),
        max_output_bytes: usize::try_from(defaults.max_output_bytes)
            .map_err(|_| SandboxError("shell output limit too large".into()))?,
    }))
}

#[cfg(test)]
mod shell_defaults_tests {
    use super::*;

    #[test]
    fn prepare_requires_typed_shell_defaults_and_rejects_invalid_values() {
        let mut request = proto::Prepare {
            tool_name: "shell".into(),
            ..Default::default()
        };
        assert!(
            shell_defaults(&request)
                .unwrap_err()
                .to_string()
                .contains("required")
        );
        request.shell_defaults = Some(proto::ShellExecutionDefaults {
            default_workdir: "nested".into(),
            default_timeout_seconds: 2,
            default_timeout_nanos: 500_000_000,
            max_output_bytes: 0,
        });
        let defaults = shell_defaults(&request).unwrap().unwrap();
        assert_eq!(defaults.default_workdir, PathBuf::from("nested"));
        assert_eq!(defaults.default_timeout, Duration::from_millis(2500));
        assert_eq!(defaults.max_output_bytes, 0);
        request
            .shell_defaults
            .as_mut()
            .unwrap()
            .default_timeout_nanos = 1_000_000_000;
        assert!(shell_defaults(&request).is_err());
        request.tool_name = "read".into();
        assert!(shell_defaults(&request).is_err());
    }
}

const fn message(body: WorkerBody) -> proto::ConnectRequest {
    proto::ConnectRequest { body: Some(body) }
}

// A prepared reply may still be retained when its invocation moves to Execute.
const MAX_OUTSTANDING: usize = 384;
const MAX_REPLY_BYTES: usize = 1024 * 1024;
// The host currently allows 15s beyond its worker budget for Prepare and Workspace.
const DELIVERY_GRACE: Duration = Duration::from_secs(15);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
const EXECUTE_TIMEOUT: Duration = Duration::from_mins(5);
const WORKSPACE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60 + 30);

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum ReplyKind {
    Prepared,
    Completed,
    Workspace,
}

#[derive(Hash, PartialEq, Eq)]
struct ReplyKey {
    kind: ReplyKind,
    id: String,
}

struct Outstanding {
    deadline: Instant,
    reply: Option<proto::ConnectRequest>,
    cancel: CancellationToken,
    sent: bool,
}

fn request_timeout(millis: u64, default: Duration) -> Duration {
    if millis == 0 {
        default
    } else {
        Duration::from_millis(millis).min(MAX_REQUEST_TIMEOUT)
    }
}

const fn key(kind: ReplyKind, id: String) -> ReplyKey {
    ReplyKey { kind, id }
}

fn publish(
    outstanding: &mut HashMap<ReplyKey, Outstanding>,
    sender: Option<&mpsc::Sender<proto::ConnectRequest>>,
    reply_key: &ReplyKey,
    mut reply: proto::ConnectRequest,
) {
    if let Some(entry) = outstanding.get_mut(reply_key)
        && entry.deadline > Instant::now()
    {
        reply = bound_reply(reply);
        entry.sent = sender.is_some_and(|sender| sender.try_send(reply.clone()).is_ok());
        entry.reply = Some(reply);
    }
}

fn flush_outstanding(
    outstanding: &mut HashMap<ReplyKey, Outstanding>,
    sender: &mpsc::Sender<proto::ConnectRequest>,
) {
    for entry in outstanding.values_mut() {
        if entry.sent || entry.deadline <= Instant::now() {
            continue;
        }
        if let Some(reply) = &entry.reply {
            match sender.try_send(reply.clone()) {
                Ok(()) => entry.sent = true,
                Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => {
                    break;
                }
            }
        }
    }
}

fn bound_reply(reply: proto::ConnectRequest) -> proto::ConnectRequest {
    match reply.body {
        Some(WorkerBody::Prepared(ref value))
            if value.invocation_id.len()
                + value.error.len()
                + value.ticket.len()
                + value
                    .claims
                    .iter()
                    .map(|claim| {
                        claim.resource.len() + claim.operation.len() + claim.resource_kind.len()
                    })
                    .sum::<usize>()
                > MAX_REPLY_BYTES =>
        {
            prepared(
                value.invocation_id.clone(),
                Err(SandboxError("worker reply too large".into())),
            )
        }
        Some(WorkerBody::Completed(ref value))
            if value.invocation_id.len()
                + value.error.len()
                + value
                    .output
                    .as_ref()
                    .map_or(0, |output| output.content.len())
                > MAX_REPLY_BYTES =>
        {
            completed(
                value.invocation_id.clone(),
                Err(SandboxError("worker reply too large".into())),
            )
        }
        Some(WorkerBody::WorkspaceResponse(ref value))
            if value.request_id.len()
                + value.content.len()
                + value.error.len()
                + value.canonical_path.len()
                + value.path_kind.len()
                + value.canonical_root.len()
                + value.entries.iter().map(String::len).sum::<usize>()
                > MAX_REPLY_BYTES =>
        {
            message(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: value.request_id.clone(),
                error: "worker reply too large".into(),
                ..Default::default()
            }))
        }
        _ => reply,
    }
}

fn reserve(
    outstanding: &mut HashMap<ReplyKey, Outstanding>,
    sender: Option<&mpsc::Sender<proto::ConnectRequest>>,
    reply_key: ReplyKey,
    timeout: Duration,
) -> bool {
    if let Some(entry) = outstanding.get(&reply_key) {
        if let (Some(reply), Some(sender)) = (&entry.reply, sender) {
            drop(sender.try_send(reply.clone()));
        }
        return false;
    }
    if outstanding.len() >= MAX_OUTSTANDING {
        return false;
    }
    outstanding.insert(
        reply_key,
        Outstanding {
            deadline: Instant::now() + timeout + DELIVERY_GRACE,
            reply: None,
            cancel: CancellationToken::new(),
            sent: false,
        },
    );
    true
}

fn cleanup_expired(
    sandbox: &Sandbox,
    outstanding: &mut HashMap<ReplyKey, Outstanding>,
    active: &HashMap<String, CancellationToken>,
) {
    let now = Instant::now();
    outstanding.retain(|key, entry| {
        if entry.deadline > now {
            return true;
        }
        entry.cancel.cancel();
        if key.kind == ReplyKind::Prepared {
            sandbox.cancel_prepared(&key.id);
        }
        if key.kind == ReplyKind::Completed
            && let Some(token) = active.get(&key.id)
        {
            token.cancel();
        }
        false
    });
}

fn completed(id: String, result: Result<crate::Execution, SandboxError>) -> proto::ConnectRequest {
    let output = match result {
        Ok(result) => proto::ToolOutput {
            content: result.content,
            is_error: result.is_error,
        },
        Err(error) => proto::ToolOutput {
            content: error.to_string(),
            is_error: true,
        },
    };
    message(WorkerBody::Completed(proto::Completed {
        invocation_id: id,
        output: Some(output),
        error: String::new(),
    }))
}

fn prepared(id: String, result: Result<crate::Prepared, SandboxError>) -> proto::ConnectRequest {
    let (claims, ticket, error) = match result {
        Ok(prepared) => (
            prepared
                .claims
                .into_iter()
                .map(|claim| proto::Claim {
                    operation: claim.operation,
                    resource_kind: claim.resource_kind,
                    resource: claim.resource,
                })
                .collect(),
            prepared.ticket,
            String::new(),
        ),
        Err(error) => (Vec::new(), String::new(), error.to_string()),
    };
    message(WorkerBody::Prepared(proto::Prepared {
        invocation_id: id,
        claims,
        ticket,
        error,
    }))
}

fn workspace_args(request: &proto::WorkspaceRequest) -> serde_json::Value {
    let search_path = if request.path.is_empty() {
        "."
    } else {
        &request.path
    };
    let pattern =
        if request.pattern.is_empty() && matches!(request.operation.as_str(), "list" | "glob") {
            "**/*"
        } else {
            &request.pattern
        };
    let mut args = serde_json::json!({
        "filePath": request.path, "path": search_path, "name": request.path, "pattern": pattern
    });
    if !request.include.is_empty() {
        args["include"] = request.include.clone().into();
    }
    if request.operation == "search_files" {
        args["limit"] = request.max_entries.into();
    }
    args
}

/// Dial the private `SandboxService` using an already configured Tonic channel.
/// A stream failure does not destroy the worker or cancel its running operations.
///
/// # Errors
/// Returns an error if the workspace root is invalid or the connection fails.
pub async fn run_worker(
    channel: Channel,
    root: impl AsRef<Path>,
    workspace_id: impl Into<String>,
) -> Result<(), SandboxError> {
    run_worker_with_shutdown(
        channel,
        root,
        workspace_id,
        CancellationToken::new(),
        Arc::new(AtomicBool::new(false)),
    )
    .await
}

/// Run a worker until host shutdown, reporting actual stream connectivity.
/// The channel's connector remains private and is reused for every dial.
///
/// # Errors
/// Returns an error if the workspace root is invalid.
#[allow(clippy::too_many_lines)]
pub async fn run_worker_with_shutdown(
    channel: Channel,
    root: impl AsRef<Path>,
    workspace_id: impl Into<String>,
    stop: CancellationToken,
    connected: Arc<AtomicBool>,
) -> Result<(), SandboxError> {
    let sandbox = Arc::new(Sandbox::new(root)?);
    let hello = message(WorkerBody::Hello(proto::Hello {
        worker_id: uuid::Uuid::new_v4().to_string(),
        workspace_id: workspace_id.into(),
        generation: 0,
        canonical_root: sandbox.root().to_string_lossy().into_owned(),
        tools: {
            let mut tools = vec![
                "read",
                "glob",
                "grep",
                "edit",
                "write",
                "delete",
                "apply_patch",
                "skill",
            ];
            if cfg!(feature = "shell") {
                tools.push("shell");
            }
            tools.into_iter().map(str::to_owned).collect()
        },
    }));
    let mut tasks = JoinSet::<(ReplyKey, proto::ConnectRequest)>::new();
    let mut active = HashMap::<String, CancellationToken>::new();
    let mut outstanding = HashMap::<ReplyKey, Outstanding>::new();
    let permits = Arc::new(Semaphore::new(16));
    let mut sender = None;
    let mut stream: Option<tonic::Streaming<proto::ConnectResponse>> = None;
    let mut delay = Duration::from_millis(50);
    let mut retry_at = time::Instant::now();
    let mut cleanup = time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok((reply_key, reply))) = joined {
                    if reply_key.kind == ReplyKind::Completed { active.remove(&reply_key.id); }
                    publish(&mut outstanding, sender.as_ref(), &reply_key, reply);
                }
            }
            _ = cleanup.tick() => {
                cleanup_expired(&sandbox, &mut outstanding, &active);
                if let Some(sender) = &sender { flush_outstanding(&mut outstanding, sender); }
            }
            incoming = async {
                if let Some(stream) = stream.as_mut() { stream.message().await } else { Ok(None) }
            }, if stream.is_some() => {
                if let Ok(Some(proto::ConnectResponse { body: Some(body) })) = incoming {
                    handle_request(body, &sandbox, &mut outstanding, sender.as_ref(), &mut active, &permits, &mut tasks);
                } else {
                    connected.store(false, Ordering::Release);
                    sender = None;
                    stream = None;
                    retry_at = time::Instant::now() + delay;
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            }
            () = time::sleep_until(retry_at), if stream.is_none() => {
                let (tx, rx) = mpsc::channel(256);
                if tx.send(hello.clone()).await.is_err() { continue; }
                let result = time::timeout(Duration::from_secs(3), SandboxServiceClient::new(channel.clone()).connect(ReceiverStream::new(rx))).await;
                if let Ok(Ok(response)) = result {
                    let next = response.into_inner();
                    // Hello is always first. Retained replies need no status query.
                    for entry in outstanding.values_mut() { entry.sent = false; }
                    flush_outstanding(&mut outstanding, &tx);
                    sender = Some(tx);
                    stream = Some(next);
                    connected.store(true, Ordering::Release);
                    delay = Duration::from_millis(50);
                } else {
                    retry_at = time::Instant::now() + delay;
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            }
        }
    }
    connected.store(false, Ordering::Release);
    for token in active.values() {
        token.cancel();
    }
    drop(
        time::timeout(Duration::from_secs(2), async {
            while tasks.join_next().await.is_some() {}
        })
        .await,
    );
    tasks.abort_all();
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn handle_request(
    body: HostBody,
    sandbox: &Arc<Sandbox>,
    outstanding: &mut HashMap<ReplyKey, Outstanding>,
    sender: Option<&mpsc::Sender<proto::ConnectRequest>>,
    active: &mut HashMap<String, CancellationToken>,
    permits: &Arc<Semaphore>,
    tasks: &mut JoinSet<(ReplyKey, proto::ConnectRequest)>,
) {
    match body {
        HostBody::Prepare(request) => {
            let id = request.invocation_id.clone();
            let reply_key = key(ReplyKind::Prepared, id.clone());
            let timeout = request_timeout(request.timeout_millis, PREPARE_TIMEOUT);
            if !reserve(outstanding, sender, reply_key, timeout) {
                return;
            }
            let canceled = outstanding[&key(ReplyKind::Prepared, id.clone())]
                .cancel
                .clone();
            let sandbox = Arc::clone(sandbox);
            tasks.spawn(async move {
                let prepared_id = id.clone();
                let blocking_sandbox = Arc::clone(&sandbox);
                let deadline = Instant::now() + timeout;
                let blocking = tokio::task::spawn_blocking(move || {
                    let result = shell_defaults(&request).and_then(|defaults| {
                        blocking_sandbox.prepare_with_shell_defaults(
                            &prepared_id,
                            &request.tool_name,
                            &request.arguments_json,
                            defaults,
                        )
                    });
                    if Instant::now() >= deadline || canceled.is_cancelled() {
                        blocking_sandbox.cancel_prepared(&prepared_id);
                    }
                    result
                });
                let result = match time::timeout(timeout, blocking).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => Err(SandboxError(error.to_string())),
                    Err(_) => Err(SandboxError("worker preparation timed out".into())),
                };
                let reply = prepared(id.clone(), result);
                if matches!(bound_reply(reply.clone()).body, Some(WorkerBody::Prepared(ref value)) if value.error == "worker reply too large")
                {
                    sandbox.cancel_prepared(&id);
                }
                (key(ReplyKind::Prepared, id), reply)
            });
        }
        HostBody::Execute(request) => {
            let id = request.invocation_id.clone();
            if active.contains_key(&id)
                && !outstanding.contains_key(&key(ReplyKind::Completed, id.clone()))
            {
                return;
            }
            let reply_key = key(ReplyKind::Completed, id.clone());
            let timeout = request_timeout(request.timeout_millis, EXECUTE_TIMEOUT);
            if !reserve(outstanding, sender, reply_key, timeout) {
                return;
            }
            let deadline = time::Instant::now() + timeout;
            let cancel = CancellationToken::new();
            active.insert(id.clone(), cancel.clone());
            let sandbox = Arc::clone(sandbox);
            let sender = sender.cloned();
            let permits = Arc::clone(permits);
            tasks.spawn(async move {
                let Some(permit) = (tokio::select! {
                    biased;
                    () = cancel.cancelled() => None,
                    acquired = time::timeout_at(deadline, permits.acquire_owned()) => acquired.ok().and_then(Result::ok),
                }) else {
                    sandbox.cancel_prepared(&id);
                    let error = if cancel.is_cancelled() { "canceled" } else { "worker execution timed out" };
                    return (key(ReplyKind::Completed, id.clone()), completed(id, Err(SandboxError(error.into()))));
                };
                if cancel.is_cancelled() || time::Instant::now() >= deadline {
                    drop(permit);
                    sandbox.cancel_prepared(&id);
                    let error = if cancel.is_cancelled() { "canceled" } else { "worker execution timed out" };
                    return (key(ReplyKind::Completed, id.clone()), completed(id, Err(SandboxError(error.into()))));
                }
                let (progress_tx, mut progress_rx) = mpsc::channel(32);
                let forward = tokio::spawn({
                    let id = id.clone();
                    async move {
                        let mut sequence = 0;
                        while let Some((level, text)) = progress_rx.recv().await {
                            sequence += 1;
                            if let Some(sender) = &sender {
                                drop(sender.try_send(message(WorkerBody::Progress(
                                    proto::Progress {
                                        invocation_id: id.clone(),
                                        sequence,
                                        level,
                                        text,
                                    },
                                ))));
                            }
                        }
                    }
                });
                let execution = sandbox.execute_with_progress(
                    &id,
                    &request.ticket,
                    cancel.clone(),
                    Some(progress_tx),
                );
                tokio::pin!(execution);
                let result = if let Ok(result) = time::timeout_at(deadline, &mut execution).await {
                    result
                } else {
                    cancel.cancel();
                    // Give shell termination and synchronous filesystem cancellation time to finish.
                    drop(time::timeout(Duration::from_secs(3), &mut execution).await);
                    Err(SandboxError("worker execution timed out".into()))
                };
                forward.abort();
                drop(permit);
                (
                    key(ReplyKind::Completed, id.clone()),
                    completed(id.clone(), result),
                )
            });
        }
        HostBody::Cancel(request) => {
            let id = request.invocation_id;
            sandbox.cancel_prepared(&id);
            if let Some(token) = active.get(&id) {
                token.cancel();
            }
            if let Some(entry) = outstanding.remove(&key(ReplyKind::Prepared, id.clone())) {
                entry.cancel.cancel();
            }
            if let Some(entry) = outstanding.remove(&key(ReplyKind::Completed, id.clone())) {
                entry.cancel.cancel();
            }
            if let Some(entry) = outstanding.remove(&key(ReplyKind::Workspace, id)) {
                entry.cancel.cancel();
            }
        }
        HostBody::WorkspaceRequest(request) => {
            let id = request.request_id.clone();
            let reply_key = key(ReplyKind::Workspace, id.clone());
            let timeout = request_timeout(request.timeout_millis, WORKSPACE_TIMEOUT);
            if !reserve(outstanding, sender, reply_key, timeout) {
                return;
            }
            let deadline = time::Instant::now() + timeout;
            let cancel = outstanding[&key(ReplyKind::Workspace, id.clone())]
                .cancel
                .clone();
            let sandbox = Arc::clone(sandbox);
            let permits = Arc::clone(permits);
            tasks.spawn(async move {
                let operation = async {
                    let _permit = permits.acquire_owned().await.map_err(|error| SandboxError(error.to_string()))?;
                    if cancel.is_cancelled() { return Err(SandboxError("worker workspace request canceled".into())); }
                    if time::Instant::now() >= deadline { return Err(SandboxError("worker workspace request timed out".into())); }
                    if request.operation == "snapshot" {
                        sandbox.snapshot(&request.path, request.max_entries as usize, request.max_depth as usize, request.max_bytes as usize).await
                    } else {
                        sandbox.workspace(&request.operation, &workspace_args(&request)).await
                    }
                };
                let result = tokio::select! {
                    biased;
                    () = cancel.cancelled() => Err(SandboxError("worker workspace request canceled".into())),
                    result = time::timeout_at(deadline, operation) => result.unwrap_or_else(|_| Err(SandboxError("worker workspace request timed out".into()))),
                };
                let response = match result {
                    Ok(result) => proto::WorkspaceResponse {
                        request_id: id.clone(),
                        content: result.content,
                        entries: result.entries,
                        canonical_path: result.canonical_path,
                        path_kind: result.path_kind,
                        canonical_root: result.canonical_root,
                        ..Default::default()
                    },
                    Err(error) => proto::WorkspaceResponse {
                        request_id: id.clone(),
                        error: error.to_string(),
                        ..Default::default()
                    },
                };
                (
                    key(ReplyKind::Workspace, id),
                    message(WorkerBody::WorkspaceResponse(response)),
                )
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn finish(
        tasks: &mut JoinSet<(ReplyKey, proto::ConnectRequest)>,
        outstanding: &mut HashMap<ReplyKey, Outstanding>,
        sender: Option<&mpsc::Sender<proto::ConnectRequest>>,
    ) {
        let (reply_key, reply) = time::timeout(Duration::from_secs(3), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        publish(outstanding, sender, &reply_key, reply);
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn losing_stream_does_not_cancel_an_active_shell() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path()).unwrap());
        let mut outstanding = HashMap::new();
        let mut active = HashMap::new();
        let permits = Arc::new(Semaphore::new(16));
        let mut tasks = JoinSet::new();
        let (tx, receiver) = mpsc::channel(8);
        let mut sender = Some(tx);
        let defaults = proto::ShellExecutionDefaults {
            default_workdir: dir.path().display().to_string(),
            default_timeout_seconds: 3,
            default_timeout_nanos: 0,
            max_output_bytes: 1024,
        };
        handle_request(HostBody::Prepare(proto::Prepare {
            invocation_id: "shell".into(), tool_name: "shell".into(),
            arguments_json: json!({"command":"touch started; sleep 0.2; printf done > finished", "description":"test"}).to_string(),
            shell_defaults: Some(defaults), timeout_millis: 1000,
        }), &sandbox, &mut outstanding, sender.as_ref(), &mut active, &permits, &mut tasks);
        finish(&mut tasks, &mut outstanding, sender.as_ref()).await;
        let ticket = match &outstanding[&key(ReplyKind::Prepared, "shell".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        {
            Some(WorkerBody::Prepared(reply)) => {
                assert!(reply.error.is_empty(), "{}", reply.error);
                reply.ticket.clone()
            }
            _ => panic!("expected prepared"),
        };
        handle_request(
            HostBody::Execute(proto::Execute {
                invocation_id: "shell".into(),
                ticket,
                timeout_millis: 2000,
            }),
            &sandbox,
            &mut outstanding,
            sender.as_ref(),
            &mut active,
            &permits,
            &mut tasks,
        );
        time::timeout(Duration::from_secs(2), async {
            while !dir.path().join("started").exists() {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        sender = None;
        drop(receiver);
        let (reply_key, reply) = time::timeout(Duration::from_secs(3), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        active.remove(&reply_key.id);
        publish(&mut outstanding, sender.as_ref(), &reply_key, reply);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("finished")).unwrap(),
            "done"
        );
        let Some(WorkerBody::Completed(reply)) = &outstanding
            [&key(ReplyKind::Completed, "shell".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        else {
            panic!("expected completed");
        };
        assert!(!reply.output.as_ref().unwrap().is_error);
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn worker_deadline_cancels_shell_without_running_the_following_write() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path()).unwrap());
        let mut outstanding = HashMap::new();
        let mut active = HashMap::new();
        let permits = Arc::new(Semaphore::new(16));
        let mut tasks = JoinSet::new();
        let prepared = sandbox
            .prepare_with_shell_defaults(
                "slow",
                "shell",
                &json!({"command":"sleep 2; touch late", "description":"deadline"}).to_string(),
                Some(ShellDefaults {
                    default_workdir: dir.path().into(),
                    default_timeout: Duration::from_secs(3),
                    max_output_bytes: 1024,
                }),
            )
            .unwrap();
        handle_request(
            HostBody::Execute(proto::Execute {
                invocation_id: "slow".into(),
                ticket: prepared.ticket,
                timeout_millis: 50,
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        finish(&mut tasks, &mut outstanding, None).await;
        let Some(WorkerBody::Completed(reply)) = &outstanding
            [&key(ReplyKind::Completed, "slow".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        else {
            panic!("expected completion");
        };
        assert!(reply.output.as_ref().unwrap().is_error);
        assert!(reply.output.as_ref().unwrap().content.contains("timed out"));
        assert!(!dir.path().join("late").exists());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn admitted_execute_and_workspace_wait_for_a_permit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readable"), "workspace result").unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path()).unwrap());
        let prepared = sandbox
            .prepare(
                "queued",
                "write",
                &json!({"filePath":"written", "content":"executed once"}).to_string(),
            )
            .unwrap();
        let mut outstanding = HashMap::new();
        let mut active = HashMap::new();
        let permits = Arc::new(Semaphore::new(16));
        let held = permits.acquire_many(16).await.unwrap();
        let mut tasks = JoinSet::new();
        let execute = proto::Execute {
            invocation_id: "queued".into(),
            ticket: prepared.ticket,
            timeout_millis: 1000,
        };
        handle_request(
            HostBody::Execute(execute.clone()),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        handle_request(
            HostBody::Execute(execute),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        handle_request(
            HostBody::WorkspaceRequest(proto::WorkspaceRequest {
                request_id: "read".into(),
                operation: "read".into(),
                path: "readable".into(),
                timeout_millis: 1000,
                ..Default::default()
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        assert_eq!(
            tasks.len(),
            2,
            "a duplicate must not enqueue another Execute"
        );
        assert!(
            outstanding[&key(ReplyKind::Completed, "queued".into())]
                .reply
                .is_none()
        );
        assert!(
            outstanding[&key(ReplyKind::Workspace, "read".into())]
                .reply
                .is_none()
        );
        assert!(!dir.path().join("written").exists());
        drop(held);
        finish(&mut tasks, &mut outstanding, None).await;
        finish(&mut tasks, &mut outstanding, None).await;
        let Some(WorkerBody::Completed(completion)) = &outstanding
            [&key(ReplyKind::Completed, "queued".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        else {
            panic!("expected completed");
        };
        assert!(!completion.output.as_ref().unwrap().is_error);
        let Some(WorkerBody::WorkspaceResponse(response)) = &outstanding
            [&key(ReplyKind::Workspace, "read".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        else {
            panic!("expected workspace response");
        };
        assert_eq!(response.content, "workspace result");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("written")).unwrap(),
            "executed once"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn queued_work_times_out_or_cancels_without_using_a_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path()).unwrap());
        let expired = sandbox
            .prepare(
                "expired",
                "write",
                &json!({"filePath":"expired", "content":"unexpected"}).to_string(),
            )
            .unwrap();
        let canceled = sandbox
            .prepare(
                "canceled",
                "write",
                &json!({"filePath":"canceled", "content":"unexpected"}).to_string(),
            )
            .unwrap();
        let mut outstanding = HashMap::new();
        let mut active = HashMap::new();
        let permits = Arc::new(Semaphore::new(16));
        let held = permits.acquire_many(16).await.unwrap();
        let mut tasks = JoinSet::new();
        handle_request(
            HostBody::Execute(proto::Execute {
                invocation_id: "expired".into(),
                ticket: expired.ticket.clone(),
                timeout_millis: 1,
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        handle_request(
            HostBody::Execute(proto::Execute {
                invocation_id: "canceled".into(),
                ticket: canceled.ticket.clone(),
                timeout_millis: 1000,
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        handle_request(
            HostBody::Cancel(proto::Cancel {
                invocation_id: "canceled".into(),
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        handle_request(
            HostBody::WorkspaceRequest(proto::WorkspaceRequest {
                request_id: "expired-read".into(),
                operation: "read".into(),
                path: "missing".into(),
                timeout_millis: 1,
                ..Default::default()
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        for _ in 0..3 {
            finish(&mut tasks, &mut outstanding, None).await;
        }
        let Some(WorkerBody::Completed(reply)) = &outstanding
            [&key(ReplyKind::Completed, "expired".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        else {
            panic!("expected completed");
        };
        assert!(reply.output.as_ref().unwrap().content.contains("timed out"));
        let Some(WorkerBody::WorkspaceResponse(reply)) = &outstanding
            [&key(ReplyKind::Workspace, "expired-read".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        else {
            panic!("expected workspace response");
        };
        assert!(reply.error.contains("timed out"));
        assert!(!outstanding.contains_key(&key(ReplyKind::Completed, "canceled".into())));
        assert!(
            sandbox
                .execute("expired", &expired.ticket, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            sandbox
                .execute("canceled", &canceled.ticket, CancellationToken::new())
                .await
                .is_err()
        );
        drop(held);
        assert!(tasks.is_empty());
        assert!(!dir.path().join("expired").exists());
        assert!(!dir.path().join("canceled").exists());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn outstanding_replies_are_bounded_replayed_and_never_execute_twice() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path()).unwrap());
        let mut outstanding = HashMap::new();
        let mut active = HashMap::new();
        let permits = Arc::new(Semaphore::new(16));
        let mut tasks = JoinSet::new();
        let (tx, mut old_stream) = mpsc::channel(8);
        let mut sender = Some(tx);
        let prepare = proto::Prepare {
            invocation_id: "one".into(),
            tool_name: "write".into(),
            arguments_json: json!({"filePath":"written", "content":"once"}).to_string(),
            ..Default::default()
        };
        handle_request(
            HostBody::Prepare(prepare.clone()),
            &sandbox,
            &mut outstanding,
            sender.as_ref(),
            &mut active,
            &permits,
            &mut tasks,
        );
        finish(&mut tasks, &mut outstanding, sender.as_ref()).await;
        let Some(WorkerBody::Prepared(prepared_reply)) = old_stream.recv().await.unwrap().body
        else {
            panic!("expected prepared");
        };
        assert!(!prepared_reply.ticket.is_empty());
        handle_request(
            HostBody::Prepare(prepare),
            &sandbox,
            &mut outstanding,
            sender.as_ref(),
            &mut active,
            &permits,
            &mut tasks,
        );
        let Some(WorkerBody::Prepared(duplicate)) = old_stream.recv().await.unwrap().body else {
            panic!("expected prepared");
        };
        assert_eq!(duplicate.ticket, prepared_reply.ticket);

        let execute = proto::Execute {
            invocation_id: "one".into(),
            ticket: prepared_reply.ticket,
            timeout_millis: 1000,
        };
        handle_request(
            HostBody::Execute(execute.clone()),
            &sandbox,
            &mut outstanding,
            sender.as_ref(),
            &mut active,
            &permits,
            &mut tasks,
        );
        handle_request(
            HostBody::Execute(execute.clone()),
            &sandbox,
            &mut outstanding,
            sender.as_ref(),
            &mut active,
            &permits,
            &mut tasks,
        );
        sender = None;
        drop(old_stream);
        let (reply_key, reply) = time::timeout(Duration::from_secs(2), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        active.remove(&reply_key.id);
        publish(&mut outstanding, sender.as_ref(), &reply_key, reply);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("written")).unwrap(),
            "once"
        );
        let (reconnected, mut stream) = mpsc::channel(8);
        for entry in outstanding.values() {
            if let Some(reply) = &entry.reply {
                reconnected.send(reply.clone()).await.unwrap();
            }
        }
        let mut kinds = Vec::new();
        for _ in 0..2 {
            kinds.push(stream.recv().await.unwrap().body.unwrap());
        }
        assert!(
            kinds
                .iter()
                .any(|body| matches!(body, WorkerBody::Prepared(_)))
        );
        assert!(
            kinds
                .iter()
                .any(|body| matches!(body, WorkerBody::Completed(_)))
        );
        sender = Some(reconnected);
        handle_request(
            HostBody::Execute(execute),
            &sandbox,
            &mut outstanding,
            sender.as_ref(),
            &mut active,
            &permits,
            &mut tasks,
        );
        assert!(matches!(
            stream.recv().await.unwrap().body,
            Some(WorkerBody::Completed(_))
        ));
        assert!(tasks.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("written")).unwrap(),
            "once"
        );
    }

    #[tokio::test]
    async fn expiry_reclaims_tickets_and_cancels_only_expired_executions() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path()).unwrap());
        let mut outstanding = HashMap::new();
        let mut active = HashMap::new();
        let permits = Arc::new(Semaphore::new(16));
        let mut tasks = JoinSet::new();
        handle_request(
            HostBody::Prepare(proto::Prepare {
                invocation_id: "expired".into(),
                tool_name: "write".into(),
                arguments_json: json!({"filePath":"marker", "content":"x"}).to_string(),
                ..Default::default()
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        finish(&mut tasks, &mut outstanding, None).await;
        let Some(WorkerBody::Prepared(prepared)) = &outstanding
            [&key(ReplyKind::Prepared, "expired".into())]
            .reply
            .as_ref()
            .unwrap()
            .body
        else {
            panic!("expected prepared");
        };
        let ticket = prepared.ticket.clone();
        outstanding
            .get_mut(&key(ReplyKind::Prepared, "expired".into()))
            .unwrap()
            .deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let cancel = CancellationToken::new();
        active.insert("other".into(), cancel.clone());
        cleanup_expired(&sandbox, &mut outstanding, &active);
        assert!(outstanding.is_empty());
        assert!(!cancel.is_cancelled());
        assert!(
            sandbox
                .execute("expired", &ticket, CancellationToken::new())
                .await
                .is_err()
        );
        outstanding.insert(
            key(ReplyKind::Completed, "other".into()),
            Outstanding {
                deadline: Instant::now()
                    .checked_sub(Duration::from_millis(1))
                    .unwrap(),
                reply: None,
                cancel: CancellationToken::new(),
                sent: false,
            },
        );
        cleanup_expired(&sandbox, &mut outstanding, &active);
        assert!(cancel.is_cancelled());
        assert!(outstanding.is_empty());
    }

    #[tokio::test]
    async fn cancel_during_preparation_does_not_leave_a_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path()).unwrap());
        let mut outstanding = HashMap::new();
        let mut active = HashMap::new();
        let permits = Arc::new(Semaphore::new(16));
        let mut tasks = JoinSet::new();
        handle_request(
            HostBody::Prepare(proto::Prepare {
                invocation_id: "cancelled".into(),
                tool_name: "write".into(),
                arguments_json: json!({"filePath":"marker", "content":"x"}).to_string(),
                ..Default::default()
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        handle_request(
            HostBody::Cancel(proto::Cancel {
                invocation_id: "cancelled".into(),
            }),
            &sandbox,
            &mut outstanding,
            None,
            &mut active,
            &permits,
            &mut tasks,
        );
        let (_, reply) = time::timeout(Duration::from_secs(2), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Some(WorkerBody::Prepared(reply)) = reply.body else {
            panic!("expected prepared");
        };
        assert!(!reply.ticket.is_empty());
        assert!(
            sandbox
                .execute("cancelled", &reply.ticket, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(outstanding.is_empty());
    }

    #[test]
    fn outbox_caps_entries_and_large_replies() {
        let mut outstanding = HashMap::new();
        for i in 0..MAX_OUTSTANDING {
            assert!(reserve(
                &mut outstanding,
                None,
                key(ReplyKind::Workspace, i.to_string()),
                WORKSPACE_TIMEOUT
            ));
        }
        assert!(!reserve(
            &mut outstanding,
            None,
            key(ReplyKind::Workspace, "overflow".into()),
            WORKSPACE_TIMEOUT
        ));
        assert_eq!(outstanding.len(), MAX_OUTSTANDING);
        let large = message(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
            request_id: "large".into(),
            content: "x".repeat(MAX_REPLY_BYTES + 1),
            ..Default::default()
        }));
        let Some(WorkerBody::WorkspaceResponse(reply)) = bound_reply(large).body else {
            panic!("expected workspace response");
        };
        assert_eq!(reply.error, "worker reply too large");
        assert!(reply.content.is_empty());
        assert_eq!(request_timeout(0, PREPARE_TIMEOUT), PREPARE_TIMEOUT);
        assert_eq!(
            request_timeout(u64::MAX, PREPARE_TIMEOUT),
            MAX_REQUEST_TIMEOUT
        );
    }

    #[test]
    fn prepare_and_workspace_replies_survive_the_host_delivery_window() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let mut outstanding = HashMap::new();
        let active = HashMap::new();
        let worker_budget = Duration::from_secs(15);
        for (reply_key, reply) in [
            (
                key(ReplyKind::Prepared, "prepare".into()),
                prepared("prepare".into(), Err(SandboxError("prepare failed".into()))),
            ),
            (
                key(ReplyKind::Workspace, "workspace".into()),
                message(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                    request_id: "workspace".into(),
                    content: "workspace result".into(),
                    ..Default::default()
                })),
            ),
        ] {
            let start = Instant::now();
            assert!(reserve(
                &mut outstanding,
                None,
                key(reply_key.kind, reply_key.id.clone()),
                worker_budget
            ));
            assert!(outstanding[&reply_key].deadline >= start + Duration::from_secs(30));
            publish(
                &mut outstanding,
                None,
                &key(reply_key.kind, reply_key.id.clone()),
                reply,
            );
            // Advance the stored deadline by 26s without waiting for wall time.
            outstanding.get_mut(&reply_key).unwrap().deadline -= Duration::from_secs(26);
        }
        cleanup_expired(&sandbox, &mut outstanding, &active);
        assert_eq!(
            outstanding.len(),
            2,
            "replies must remain available after 25s"
        );
        let (sender, mut receiver) = mpsc::channel(2);
        flush_outstanding(&mut outstanding, &sender);
        let mut replayed = Vec::new();
        while let Ok(reply) = receiver.try_recv() {
            replayed.push(reply.body.unwrap());
        }
        assert!(replayed.iter().any(
            |body| matches!(body, WorkerBody::Prepared(reply) if reply.invocation_id == "prepare")
        ));
        assert!(replayed.iter().any(|body| matches!(body, WorkerBody::WorkspaceResponse(reply) if reply.request_id == "workspace")));
        for entry in outstanding.values_mut() {
            entry.deadline -= Duration::from_secs(5);
        }
        cleanup_expired(&sandbox, &mut outstanding, &active);
        assert!(
            outstanding.is_empty(),
            "replies must still be cleaned up after the host deadline"
        );
    }

    #[tokio::test]
    async fn full_stream_queue_does_not_lose_a_new_reply() {
        let mut outstanding = HashMap::new();
        let (tx, mut receiver) = mpsc::channel(1);
        tx.send(message(WorkerBody::Progress(proto::Progress::default())))
            .await
            .unwrap();
        assert!(reserve(
            &mut outstanding,
            Some(&tx),
            key(ReplyKind::Workspace, "id".into()),
            WORKSPACE_TIMEOUT
        ));
        publish(
            &mut outstanding,
            Some(&tx),
            &key(ReplyKind::Workspace, "id".into()),
            message(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: "id".into(),
                ..Default::default()
            })),
        );
        assert!(!outstanding[&key(ReplyKind::Workspace, "id".into())].sent);
        drop(receiver.recv().await);
        flush_outstanding(&mut outstanding, &tx);
        assert!(outstanding[&key(ReplyKind::Workspace, "id".into())].sent);
        assert!(
            matches!(receiver.recv().await.unwrap().body, Some(WorkerBody::WorkspaceResponse(reply)) if reply.request_id == "id")
        );
    }

    fn assert_tool_failure(reply: proto::ConnectRequest, id: &str, expected: &str) {
        let Some(WorkerBody::Completed(reply)) = reply.body else {
            panic!("expected completed worker result");
        };
        assert_eq!(reply.invocation_id, id);
        assert!(reply.error.is_empty());
        let output = reply.output.expect("known terminal output");
        assert!(output.is_error);
        assert!(output.content.contains(expected), "{}", output.content);
    }

    #[tokio::test]
    async fn malformed_edit_and_patch_conflict_are_terminal_tool_outputs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "original\n").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();

        let edit = sandbox
            .prepare(
                "edit",
                "edit",
                &json!({"filePath": "note.txt", "oldString": "missing", "newString": "replacement"}).to_string(),
            )
            .unwrap();
        assert_tool_failure(
            completed(
                "edit".into(),
                sandbox
                    .execute("edit", &edit.ticket, CancellationToken::new())
                    .await,
            ),
            "edit",
            "expected exactly one match for edit, found 0",
        );

        let patch = sandbox
            .prepare(
                "patch",
                "apply_patch",
                &json!({"patchText": "*** Begin Patch\n*** Update File: note.txt\n@@\n-original\n+replacement\n*** End Patch"}).to_string(),
            )
            .unwrap();
        std::fs::write(dir.path().join("note.txt"), "changed since preparation\n").unwrap();
        assert_tool_failure(
            completed(
                "patch".into(),
                sandbox
                    .execute("patch", &patch.ticket, CancellationToken::new())
                    .await,
            ),
            "patch",
            "failed to find expected lines",
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "changed since preparation\n"
        );
    }

    #[test]
    fn rejected_execute_is_a_known_result_but_prepare_failure_is_separate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "original\n").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepare_error = sandbox
            .prepare("bad", "edit", &json!({"filePath": "note.txt"}).to_string())
            .unwrap_err();
        assert_eq!(prepare_error.to_string(), "missing or invalid oldString");
        let Some(WorkerBody::Prepared(reply)) = prepared("bad".into(), Err(prepare_error)).body
        else {
            panic!("expected prepared worker result");
        };
        assert_eq!(reply.invocation_id, "bad");
        assert_eq!(reply.error, "missing or invalid oldString");
        assert!(reply.ticket.is_empty());
        assert!(reply.claims.is_empty());
        assert_tool_failure(
            completed(
                "timeout".into(),
                Err(SandboxError("worker execution timed out".into())),
            ),
            "timeout",
            "worker execution timed out",
        );
        assert_tool_failure(
            completed(
                "duplicate".into(),
                Err(SandboxError("invocation already running".into())),
            ),
            "duplicate",
            "invocation already running",
        );
    }

    #[test]
    fn successful_execution_preserves_tool_output() {
        let Some(WorkerBody::Completed(reply)) = completed(
            "success".into(),
            Ok(crate::Execution {
                content: "done".into(),
                is_error: false,
            }),
        )
        .body
        else {
            panic!("expected completed worker result");
        };
        assert_eq!(reply.invocation_id, "success");
        assert!(reply.error.is_empty());
        let output = reply.output.unwrap();
        assert_eq!(output.content, "done");
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn workspace_wire_defaults_do_not_filter_searches_or_format_instruction_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Instructions\n").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let read = proto::WorkspaceRequest {
            operation: "read".into(),
            path: "AGENTS.md".into(),
            ..Default::default()
        };
        assert_eq!(
            sandbox
                .workspace("read", &workspace_args(&read))
                .await
                .unwrap()
                .content,
            "# Instructions\n"
        );
        let list = proto::WorkspaceRequest {
            operation: "list".into(),
            ..Default::default()
        };
        assert_eq!(
            sandbox
                .workspace("list", &workspace_args(&list))
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        let grep = proto::WorkspaceRequest {
            operation: "search".into(),
            pattern: "Instructions".into(),
            ..Default::default()
        };
        assert!(
            sandbox
                .workspace("search", &workspace_args(&grep))
                .await
                .unwrap()
                .content
                .contains("Found 1 matches")
        );
        let files = proto::WorkspaceRequest {
            operation: "search_files".into(),
            pattern: "AGENTS".into(),
            max_entries: 1,
            ..Default::default()
        };
        assert_eq!(workspace_args(&files)["limit"], 1);
        assert_eq!(
            sandbox
                .workspace("search_files", &workspace_args(&files))
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn workspace_wire_info_and_resolve_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Instructions\n").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let read = proto::WorkspaceRequest {
            operation: "read".into(),
            path: "AGENTS.md".into(),
            ..Default::default()
        };
        let info = sandbox
            .workspace("info", &workspace_args(&proto::WorkspaceRequest::default()))
            .await
            .unwrap();
        assert_eq!(info.canonical_root, sandbox.root().to_string_lossy());
        assert_eq!(info.path_kind, "directory");
        let resolved = sandbox
            .workspace("resolve", &workspace_args(&read))
            .await
            .unwrap();
        assert_eq!(
            resolved.canonical_path,
            sandbox.root().join("AGENTS.md").to_string_lossy()
        );
        assert_eq!(resolved.path_kind, "file");
        assert!(
            sandbox
                .workspace(
                    "resolve",
                    &workspace_args(&proto::WorkspaceRequest {
                        path: "missing".into(),
                        ..Default::default()
                    })
                )
                .await
                .is_err()
        );
    }
}
