//! Temporary, opt-in local diagnostics; never routed through tracing or OTLP.

use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(super) enum Stage {
    RequestStart,
    Serialized,
    Send,
    Headers,
    ErrorBodyStart,
    ErrorBodyEnd,
    Open,
    FirstChunk,
    FirstSse,
    FirstParsed,
    FirstEvent,
    Progress,
    Finished,
    Eof,
    Failed,
    Dropped,
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    None,
    Timeout,
    Connect,
    Body,
    Decode,
    Request,
    OtherHttp,
    Parse,
}

pub(super) struct Diagnostics {
    id: Option<u64>,
    tools: usize,
    status: u16,
    chunks: u64,
    sse: u64,
    events: u64,
    parsed: bool,
    terminal: bool,
    failure: Failure,
    progress: Instant,
}

impl Diagnostics {
    pub(super) fn new(tools: usize) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let enabled = cfg!(debug_assertions)
            && std::env::var_os("WALTR_LOCAL_PROVIDER_DIAGNOSTICS")
                .is_some_and(|value| value == "1");
        let diagnostics = Self {
            id: enabled.then(|| NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            tools,
            status: 0,
            chunks: 0,
            sse: 0,
            events: 0,
            parsed: false,
            terminal: false,
            failure: Failure::None,
            progress: Instant::now(),
        };
        diagnostics.emit(Stage::RequestStart);
        diagnostics
    }

    pub(super) fn stage(&mut self, stage: Stage) {
        self.terminal |= matches!(stage, Stage::Finished | Stage::Eof | Stage::Failed);
        self.emit(stage);
    }

    pub(super) fn headers(&mut self, status: u16) {
        self.status = status;
        self.emit(Stage::Headers);
    }

    pub(super) fn http_error(&mut self, error: &reqwest::Error) {
        self.failure = if error.is_timeout() {
            Failure::Timeout
        } else if error.is_connect() {
            Failure::Connect
        } else if error.is_body() {
            Failure::Body
        } else if error.is_decode() {
            Failure::Decode
        } else if error.is_request() {
            Failure::Request
        } else {
            Failure::OtherHttp
        };
        self.stage(Stage::Failed);
    }

    pub(super) fn parse_error(&mut self) {
        self.failure = Failure::Parse;
        self.stage(Stage::Failed);
    }

    pub(super) fn chunk(&mut self) {
        self.chunks += 1;
        if self.chunks == 1 {
            self.emit(Stage::FirstChunk);
        }
        self.progress();
    }

    pub(super) fn sse(&mut self) {
        self.sse += 1;
        if self.sse == 1 {
            self.emit(Stage::FirstSse);
        }
        self.progress();
    }

    pub(super) fn parsed(&mut self) {
        if !self.parsed {
            self.parsed = true;
            self.emit(Stage::FirstParsed);
        }
    }

    pub(super) fn event(&mut self) {
        self.events += 1;
        if self.events == 1 {
            self.emit(Stage::FirstEvent);
        }
        self.progress();
    }

    fn progress(&mut self) {
        if self.id.is_some() && self.progress.elapsed() >= Duration::from_secs(5) {
            self.progress = Instant::now();
            self.emit(Stage::Progress);
        }
    }

    fn emit(&self, stage: Stage) {
        if let Some(id) = self.id {
            // Only enums and numeric fields cross this boundary, never span context.
            drop(writeln!(std::io::stderr().lock(), "{}", self.line(id, stage)));
        }
    }

    fn line(&self, id: u64, stage: Stage) -> String {
        format!(
            "waltr_local_provider id={id} stage={stage:?} tools={} status={} chunks={} sse={} events={} failure={:?}",
            self.tools, self.status, self.chunks, self.sse, self.events, self.failure
        )
    }
}

impl Drop for Diagnostics {
    fn drop(&mut self) {
        if !self.terminal {
            self.emit(Stage::Dropped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_diagnostics_format_has_only_fixed_fields() {
        let mut diagnostics = Diagnostics::new(23);
        diagnostics.id = None;
        diagnostics.status = 200;
        assert_eq!(
            diagnostics.line(7, Stage::Headers),
            "waltr_local_provider id=7 stage=Headers tools=23 status=200 chunks=0 sse=0 events=0 failure=None"
        );
    }

    #[test]
    fn local_diagnostics_counts_and_terminal_state() {
        let mut diagnostics = Diagnostics::new(0);
        diagnostics.id = None;
        diagnostics.chunk();
        diagnostics.sse();
        diagnostics.parsed();
        diagnostics.event();
        diagnostics.stage(Stage::Finished);
        assert_eq!(
            (diagnostics.chunks, diagnostics.sse, diagnostics.events),
            (1, 1, 1)
        );
        assert!(diagnostics.parsed);
        assert!(diagnostics.terminal);
    }

    #[test]
    fn local_diagnostics_http_error_does_not_format_source() {
        let mut diagnostics = Diagnostics::new(0);
        diagnostics.id = None;
        let error = reqwest::Client::new()
            .get("not-a-url-secret-payload")
            .build()
            .expect_err("invalid URL must fail without sending");
        diagnostics.http_error(&error);
        assert_eq!(
            diagnostics.line(1, Stage::Failed),
            "waltr_local_provider id=1 stage=Failed tools=0 status=0 chunks=0 sse=0 events=0 failure=OtherHttp"
        );
        assert!(diagnostics.terminal);
    }
}
