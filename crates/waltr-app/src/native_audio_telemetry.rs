//! Content-free native audio diagnostics. All labels are fixed literals, never IPC/config values.
use crate::audio_permissions::PermissionResult;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;
use tracing::Span;
use waltr_desktop::{
    capture,
    host::{Diagnostic, Error, Observer, RpcFailure, Stage},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
const TARGET: &str = "waltr_app_lib::dictation_telemetry";

#[derive(Clone)]
pub struct Operation(Arc<Inner>);
struct Inner {
    span: Span,
    dispatch: tracing::Dispatch,
    started: Instant,
    entrypoint: &'static str,
    id: u64,
    snapshot: Mutex<State>,
}
struct State {
    seen: u16,
    stage: &'static str,
    primary: &'static str,
    failure_stage: &'static str,
    capture_cleanup: &'static str,
    typing_cleanup: &'static str,
    rpc: Option<(&'static str, &'static str)>,
    terminal: bool,
}
pub fn admission(entrypoint: &'static str, outcome: &'static str) {
    tracing::event!(target: TARGET, tracing::Level::INFO, { event.name = "native.audio.admission", entrypoint = entrypoint, outcome = outcome });
}
impl Operation {
    pub fn new(entrypoint: &'static str) -> Self {
        // Saturate rather than wrap; no stable identity is derived from the caller.
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                Some(id.saturating_add(1))
            })
            .unwrap_or(u64::MAX);
        let span = tracing::info_span!(target: TARGET, "native.audio.operation", entrypoint,
            diagnostic_id = id, outcome = tracing::field::Empty, category = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty, stage = tracing::field::Empty,
            capture_cleanup = tracing::field::Empty, typing_cleanup = tracing::field::Empty,
            failure_stage = tracing::field::Empty);
        Self(Arc::new(Inner {
            span,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            started: Instant::now(),
            entrypoint,
            id,
            snapshot: Mutex::new(State {
                seen: 0,
                stage: "permission",
                primary: "none",
                failure_stage: "none",
                capture_cleanup: "not_needed",
                typing_cleanup: "not_needed",
                rpc: None,
                terminal: false,
            }),
        }))
    }
    pub fn span(&self) -> Span {
        self.0.span.clone()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn elapsed(&self) -> u64 {
        u64::try_from(self.0.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn in_scope<T>(&self, emit: impl FnOnce() -> T) -> T {
        tracing::dispatcher::with_default(&self.0.dispatch, || self.0.span.in_scope(emit))
    }
    pub fn observer(&self) -> Observer {
        let operation = self.clone();
        Observer::new(move |event| operation.observe(event))
    }
    pub fn permission(&self, result: PermissionResult) {
        self.in_scope(|| self.permission_scoped(result));
    }
    fn permission_scoped(&self, result: PermissionResult) {
        let category = match result {
            PermissionResult::Granted => "granted",
            PermissionResult::Denied => "permission_denied",
            PermissionResult::Unavailable => "permission_unavailable",
            PermissionResult::Busy => "permission_busy",
            PermissionResult::Cancelled => "cancelled",
        };
        tracing::event!(target: TARGET, parent: &self.0.span, tracing::Level::INFO, { event.name = "native.audio.permission",
            entrypoint = self.0.entrypoint, diagnostic_id = self.0.id, category = category, elapsed_ms = self.elapsed() });
        if result != PermissionResult::Granted {
            self.fail(category);
        }
    }
    pub fn fail(&self, category: &'static str) {
        let mut snapshot = self.lock();
        if matches!(snapshot.primary, "none" | "no_input") {
            snapshot.primary = category;
            snapshot.failure_stage = snapshot.stage;
        }
    }
    #[cfg(test)]
    pub fn stage(&self, stage: Stage) {
        self.observe(Diagnostic::Stage(stage));
    }
    fn observe(&self, event: Diagnostic) {
        self.in_scope(|| self.observe_scoped(event));
    }
    fn observe_scoped(&self, event: Diagnostic) {
        let mut snapshot = self.lock();
        if snapshot.terminal {
            return;
        }
        match event {
            Diagnostic::Stage(stage) => {
                let bit = 1 << stage as u16;
                if snapshot.seen & bit != 0 {
                    return;
                }
                snapshot.seen |= bit;
                snapshot.stage = match stage {
                    Stage::CaptureRequested => "capture_requested",
                    Stage::CaptureReady => "capture_ready",
                    Stage::FirstFrameObserved => "first_frame_observed",
                    Stage::RpcOpen => "rpc_open",
                    Stage::Accepted => "accepted",
                    Stage::InputFinished => "input_finished",
                    Stage::OutputFinished => "output_finished",
                    Stage::Action => "action",
                    Stage::Cleanup => "cleanup",
                };
                let stage = snapshot.stage;
                drop(snapshot);
                tracing::event!(target: TARGET, parent: &self.0.span, tracing::Level::INFO, { event.name = "native.audio.milestone",
                    entrypoint = self.0.entrypoint, diagnostic_id = self.0.id, stage = stage, elapsed_ms = self.elapsed() });
            }
            Diagnostic::Primary(Err(error)) => {
                let (category, failure_stage) = if error == Error::Interaction {
                    snapshot.rpc.unwrap_or(("interaction", snapshot.stage))
                } else {
                    (classify(error), snapshot.stage)
                };
                // Owned pipeline failures are observed before cleanup; later aggregate results
                // must not promote a cleanup-only failure into the primary category.
                if matches!(snapshot.primary, "none" | "no_input")
                    && category != snapshot.capture_cleanup
                    && category != snapshot.typing_cleanup
                {
                    snapshot.primary = category;
                    snapshot.failure_stage = failure_stage;
                }
            }
            Diagnostic::NoInput => {
                if matches!(snapshot.primary, "none" | "no_input") {
                    snapshot.primary = "no_input";
                }
            }
            Diagnostic::Primary(Ok(())) => {}
            Diagnostic::CaptureCleanup(result) => {
                // A repeated no-op joined check must not erase a prior failure.
                if snapshot.capture_cleanup == "not_needed" || snapshot.capture_cleanup == "ok" {
                    snapshot.capture_cleanup = result.map_or_else(capture_category, |()| "ok");
                }
            }
            Diagnostic::TypingCleanup(result) => {
                snapshot.typing_cleanup = result.map_or_else(typing_category, |()| "ok");
            }
            Diagnostic::RpcFailure(error) => {
                if snapshot.rpc.is_none() {
                    let category = match error {
                        RpcFailure::Open => "rpc_open",
                        RpcFailure::AcceptanceTimeout => "rpc_acceptance_timeout",
                        RpcFailure::Input => "rpc_input",
                        RpcFailure::Output => "rpc_output",
                        RpcFailure::Protocol => "rpc_protocol",
                        RpcFailure::RemoteFailed => "rpc_remote_failed",
                        RpcFailure::RemoteInterrupted => "rpc_remote_interrupted",
                        RpcFailure::Rejected => "rpc_rejected",
                        RpcFailure::LoginRequired => "rpc_login_required",
                    };
                    snapshot.rpc = Some((category, snapshot.stage));
                }
            }
        }
    }
    pub fn primary(&self, result: Result<(), Error>) {
        self.observe(Diagnostic::Primary(result));
    }
    #[cfg(test)]
    pub fn capture_cleanup(&self, result: Result<(), capture::Error>) {
        self.observe(Diagnostic::CaptureCleanup(result));
    }
    pub fn finish(&self) {
        self.in_scope(|| self.finish_scoped());
    }
    fn finish_scoped(&self) {
        let mut snapshot = self.lock();
        if snapshot.terminal {
            return;
        }
        snapshot.terminal = true;
        let primary = snapshot.primary;
        let failure_stage = snapshot.failure_stage;
        let capture_cleanup = snapshot.capture_cleanup;
        let typing_cleanup = snapshot.typing_cleanup;
        let stage = snapshot.stage;
        drop(snapshot);
        let cleanup_failed = [capture_cleanup, typing_cleanup].iter().any(|value| {
            !matches!(
                *value,
                "ok" | "not_needed" | "cancelled" | "typing_guard_or_cancelled"
            )
        });
        let outcome = if matches!(
            primary,
            "cancelled" | "invalidated" | "typing_guard_or_cancelled"
        ) {
            "cancelled"
        } else if primary == "no_input" && !cleanup_failed {
            "no_input"
        } else if primary != "none" || cleanup_failed {
            "failed"
        } else {
            "succeeded"
        };
        let elapsed_ms = self.elapsed();
        self.0.span.record("outcome", outcome);
        self.0.span.record("category", primary);
        self.0.span.record("elapsed_ms", elapsed_ms);
        self.0.span.record("stage", stage);
        self.0.span.record("failure_stage", failure_stage);
        self.0.span.record("capture_cleanup", capture_cleanup);
        self.0.span.record("typing_cleanup", typing_cleanup);
        if outcome == "failed" || cleanup_failed {
            tracing::event!(target: TARGET, parent: &self.0.span, tracing::Level::WARN, { event.name = "native.audio.finished",
                entrypoint = self.0.entrypoint, diagnostic_id = self.0.id, outcome = outcome, category = primary,
                capture_cleanup = capture_cleanup, typing_cleanup = typing_cleanup, stage = stage, failure_stage = failure_stage, elapsed_ms = elapsed_ms });
        } else {
            tracing::event!(target: TARGET, parent: &self.0.span, tracing::Level::INFO, { event.name = "native.audio.finished",
                entrypoint = self.0.entrypoint, diagnostic_id = self.0.id, outcome = outcome, category = primary,
                capture_cleanup = capture_cleanup, typing_cleanup = typing_cleanup, stage = stage, failure_stage = failure_stage, elapsed_ms = elapsed_ms });
        }
    }
}
#[cfg(test)]
pub const fn rpc_category(
    error: &waltr_client::ClientError,
    boundary: &'static str,
) -> &'static str {
    if error.is_waltr_login_required() {
        "rpc_login_required"
    } else {
        boundary
    }
}
pub const fn capture_category(error: capture::Error) -> &'static str {
    match error {
        capture::Error::Unsupported => "capture_unsupported",
        capture::Error::PreacceptBudgetExceeded => "capture_budget",
        capture::Error::Backend => "capture_backend",
        capture::Error::CallbackOverflow => "capture_callback_overflow",
        capture::Error::CallbackPoolExhausted => "capture_callback_pool_exhausted",
        capture::Error::CallbackQueueFull => "capture_callback_queue_full",
        capture::Error::Conversion => "capture_conversion",
        capture::Error::ResourceLimit => "capture_resource_limit",
        capture::Error::Worker => "capture_worker",
        capture::Error::Overflow => "capture_host_overflow",
        capture::Error::MalformedFrame => "capture_malformed_frame",
        capture::Error::ReceiverClosed => "capture_receiver_closed",
        capture::Error::Cancelled => "cancelled",
    }
}
const fn typing_category(error: waltr_desktop::typing::Error) -> &'static str {
    use waltr_desktop::typing::Error as E;
    match error {
        E::Unsupported => "typing_unsupported",
        E::Cancelled => "typing_guard_or_cancelled",
        E::Control => "typing_control",
        E::Limit => "typing_limit",
        E::Revision => "typing_revision",
        E::Backend => "typing_backend",
    }
}
const fn classify(error: Error) -> &'static str {
    use waltr_desktop::command::CommandError as C;
    match error {
        Error::Busy => "busy",
        Error::Unsupported => "unsupported",
        Error::Cancelled => "cancelled",
        Error::Capture(error) => capture_category(error),
        Error::Interaction => "interaction",
        Error::Typing(error) => typing_category(error),
        Error::Command(error) => match error {
            C::Invalid(_) => "command_invalid",
            C::InputLimit => "command_input_limit",
            C::ExpandedLimit => "command_expanded_limit",
            C::Cancelled => "cancelled",
            C::Timeout => "command_timeout",
            C::OutputLimit { .. } => "command_output_limit",
            C::Io { .. } => "command_io",
            C::Exit { .. } => "command_exit",
            C::Cleanup { .. } => "command_cleanup",
        },
    }
}

#[cfg(test)]
mod isolation {
    use std::{
        io::Read,
        process::{Command, Output, Stdio},
        time::{Duration, Instant},
    };

    const CHILD: &str = "WALTR_AUDIO_TELEMETRY_TEST_CHILD";
    const TIMEOUT: Duration = Duration::from_secs(30);

    pub(super) fn is_child(name: &str) -> bool {
        match std::env::var(CHILD) {
            Ok(child) => {
                assert_eq!(child, name, "isolated child must run only its exact test");
                true
            }
            Err(std::env::VarError::NotPresent) => false,
            Err(error) => panic!("invalid isolated child marker: {error}"),
        }
    }

    fn drain(mut pipe: impl Read) -> Vec<u8> {
        let mut retained = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = pipe.read(&mut buffer).expect("read isolated test output");
            if count == 0 {
                return retained;
            }
            let keep = count.min((64 * 1024_usize).saturating_sub(retained.len()));
            retained.extend_from_slice(&buffer[..keep]);
        }
    }

    pub(super) fn run(name: &str) -> Output {
        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", name, "--test-threads=1", "--nocapture"])
            .env(CHILD, name)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start isolated telemetry test");
        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");
        let stdout = std::thread::spawn(move || drain(stdout));
        let stderr = std::thread::spawn(move || drain(stderr));
        let started = Instant::now();
        let (status, timed_out) = loop {
            if let Some(status) = child.try_wait().expect("poll isolated test") {
                break (status, false);
            }
            if started.elapsed() >= TIMEOUT {
                let _killed = child.kill();
                break (child.wait().expect("reap timed-out isolated test"), true);
            }
            std::thread::yield_now();
        };
        let output = Output {
            status,
            stdout: stdout.join().expect("join stdout reader"),
            stderr: stderr.join().expect("join stderr reader"),
        };
        assert!(
            !timed_out,
            "isolated telemetry test timed out: {name}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    pub(super) fn assert_success(name: &str, output: &Output) {
        assert!(
            output.status.success(),
            "isolated telemetry test failed: {name}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "isolated telemetry test must execute exactly one successful test: {name}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Run exporter assertions in a fresh process, before any subscriber or operation exists.
#[cfg(test)]
pub(crate) fn isolated_exporter_test(name: &str) -> bool {
    if isolation::is_child(name) {
        return true;
    }
    isolation::assert_success(name, &isolation::run(name));
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_exporter_child_failure_reaches_parent() {
        const NAME: &str =
            "native_audio_telemetry::tests::isolated_exporter_child_failure_reaches_parent";
        if isolation::is_child(NAME) {
            panic!("isolated exporter assertion sentinel");
        }
        let output = isolation::run(NAME);
        assert!(!output.status.success());
        let failure = std::panic::catch_unwind(|| isolation::assert_success(NAME, &output))
            .expect_err("child assertion failure must fail its parent assertion");
        let message = failure.downcast_ref::<String>().expect("assertion message");
        assert!(message.contains(NAME));
        assert!(message.contains("isolated exporter assertion sentinel"));
        assert!(message.contains("1 failed"));
    }

    #[test]
    fn capture_subcategories_are_finite_and_keep_primary_separate_from_cleanup() {
        for (error, category) in [
            (
                capture::Error::CallbackOverflow,
                "capture_callback_overflow",
            ),
            (
                capture::Error::CallbackPoolExhausted,
                "capture_callback_pool_exhausted",
            ),
            (
                capture::Error::CallbackQueueFull,
                "capture_callback_queue_full",
            ),
            (capture::Error::Conversion, "capture_conversion"),
            (capture::Error::ResourceLimit, "capture_resource_limit"),
            (capture::Error::Worker, "capture_worker"),
            (capture::Error::Overflow, "capture_host_overflow"),
            (capture::Error::MalformedFrame, "capture_malformed_frame"),
            (capture::Error::ReceiverClosed, "capture_receiver_closed"),
            (capture::Error::Backend, "capture_backend"),
        ] {
            let operation = Operation::new("chat");
            operation.stage(Stage::FirstFrameObserved);
            operation.primary(Err(Error::Capture(error)));
            operation.capture_cleanup(Err(error));
            operation.finish();
            let snapshot = operation.lock();
            assert_eq!(snapshot.primary, category);
            assert_eq!(snapshot.capture_cleanup, category);
            assert_eq!(snapshot.failure_stage, "first_frame_observed");
        }
    }

    #[test]
    fn no_input_is_content_free_and_cannot_mask_invalidation() {
        let operation = Operation::new("chat");
        operation.observer().emit(Diagnostic::NoInput);
        assert_eq!(operation.lock().primary, "no_input");
        operation.fail("invalidated");
        operation.capture_cleanup(Ok(()));
        operation.finish();
        assert_eq!(operation.lock().primary, "invalidated");
    }

    #[test]
    fn capture_primary_and_cleanup_remain_independent() {
        for primary in [true, false] {
            let operation = Operation::new("hold");
            operation.stage(Stage::FirstFrameObserved);
            if primary {
                operation.primary(Err(Error::Capture(capture::Error::Backend)));
            } else {
                operation.primary(Ok(()));
            }
            operation.stage(Stage::Cleanup);
            operation.capture_cleanup(Err(capture::Error::Backend));
            // The aggregate hold result may repeat a cleanup failure after teardown.
            operation.primary(Err(Error::Capture(capture::Error::Backend)));
            operation.finish();
            let snapshot = operation.lock();
            assert_eq!(
                snapshot.primary,
                if primary { "capture_backend" } else { "none" }
            );
            assert_eq!(
                snapshot.failure_stage,
                if primary {
                    "first_frame_observed"
                } else {
                    "none"
                }
            );
            assert_eq!(snapshot.capture_cleanup, "capture_backend");
        }
    }

    #[test]
    fn acceptance_timeout_has_a_distinct_content_free_category() {
        let operation = Operation::new("hold");
        let observer = operation.observer();
        observer.emit(Diagnostic::Stage(Stage::RpcOpen));
        observer.emit(Diagnostic::RpcFailure(RpcFailure::AcceptanceTimeout));
        observer.emit(Diagnostic::Primary(Err(Error::Interaction)));
        operation.finish();
        let snapshot = operation.lock();
        assert_eq!(snapshot.primary, "rpc_acceptance_timeout");
        assert_eq!(snapshot.failure_stage, "rpc_open");
    }

    #[test]
    fn native_audio_rpc_failure_preserves_observed_stage() {
        let operation = Operation::new("hold");
        let observer = operation.observer();
        observer.emit(Diagnostic::Stage(Stage::Accepted));
        observer.emit(Diagnostic::RpcFailure(RpcFailure::Protocol));
        observer.emit(Diagnostic::Stage(Stage::CaptureReady));
        observer.emit(Diagnostic::Primary(Err(Error::Interaction)));
        operation.finish();
        let snapshot = operation.lock();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.primary, "rpc_protocol");
        assert_eq!(snapshot.failure_stage, "accepted");
        assert_eq!(snapshot.stage, "capture_ready");
    }

    #[test]
    fn native_audio_first_rpc_failure_candidate_is_immutable() {
        let operation = Operation::new("hold");
        let observer = operation.observer();
        observer.emit(Diagnostic::Stage(Stage::Accepted));
        observer.emit(Diagnostic::RpcFailure(RpcFailure::Protocol));
        observer.emit(Diagnostic::Stage(Stage::CaptureReady));
        observer.emit(Diagnostic::RpcFailure(RpcFailure::Output));
        observer.emit(Diagnostic::Primary(Err(Error::Interaction)));
        operation.finish();
        let snapshot = operation.lock();
        assert_eq!(snapshot.rpc, Some(("rpc_protocol", "accepted")));
        assert_eq!(snapshot.primary, "rpc_protocol");
        assert_eq!(snapshot.failure_stage, "accepted");
        assert_eq!(snapshot.stage, "capture_ready");
    }

    #[test]
    fn native_audio_rpc_candidate_does_not_replace_other_primary_or_cleanup() {
        let operation = Operation::new("hold");
        let observer = operation.observer();
        observer.emit(Diagnostic::Stage(Stage::Accepted));
        observer.emit(Diagnostic::RpcFailure(RpcFailure::Protocol));
        observer.emit(Diagnostic::Stage(Stage::CaptureReady));
        observer.emit(Diagnostic::Primary(Err(Error::Capture(
            capture::Error::Overflow,
        ))));
        observer.emit(Diagnostic::Stage(Stage::Cleanup));
        observer.emit(Diagnostic::CaptureCleanup(Err(capture::Error::Backend)));
        observer.emit(Diagnostic::TypingCleanup(Err(
            waltr_desktop::typing::Error::Backend,
        )));
        observer.emit(Diagnostic::Primary(Err(Error::Interaction)));
        operation.finish();
        let snapshot = operation.lock();
        assert_eq!(snapshot.primary, "capture_host_overflow");
        assert_eq!(snapshot.failure_stage, "capture_ready");
        assert_eq!(snapshot.stage, "cleanup");
        assert_eq!(snapshot.capture_cleanup, "capture_backend");
        assert_eq!(snapshot.typing_cleanup, "typing_backend");
    }
}
