use std::{
    ffi::OsString,
    fmt,
    future::pending,
    path::Path,
    process::{Child, Command, Stdio},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
const QUEUE_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioExit {
    Shutdown,
    Cancelled,
    Expired,
    InputDropped,
    OutputDropped,
    StdoutClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioError {
    InvalidLaunch,
    InvalidFrameLimit,
    Inactive(StdioExit),
    RuntimeUnavailable,
    Spawn,
    MissingPipe,
    InputTooLarge,
    InputContainsNewline,
    InputClosed,
    Write,
    Read,
    OutputTooLarge,
    UnterminatedOutput,
    Terminate,
    Wait,
    Supervisor,
}

impl fmt::Display for StdioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLaunch => "invalid worker stdio launch",
            Self::InvalidFrameLimit => "invalid worker stdio frame limit",
            Self::Inactive(_) => "worker stdio lease is inactive",
            Self::RuntimeUnavailable => "worker stdio runtime is unavailable",
            Self::Spawn => "worker stdio child failed to start",
            Self::MissingPipe => "worker stdio child has a missing pipe",
            Self::InputTooLarge => "worker stdio input frame exceeds the limit",
            Self::InputContainsNewline => "worker stdio input frame contains a newline",
            Self::InputClosed => "worker stdio input is closed",
            Self::Write => "worker stdio pipe write failed",
            Self::Read => "worker stdio pipe read failed",
            Self::OutputTooLarge => "worker stdio output frame exceeds the limit",
            Self::UnterminatedOutput => "worker stdio output frame has no newline terminator",
            Self::Terminate => "worker stdio child termination failed",
            Self::Wait => "worker stdio child wait failed",
            Self::Supervisor => "worker stdio supervisor failed",
        })
    }
}

impl std::error::Error for StdioError {}

pub struct StdioInput {
    queue: mpsc::Sender<Vec<u8>>,
    max_frame_bytes: usize,
}

impl StdioInput {
    /// Queue an opaque frame with no LF. Success does not acknowledge a pipe write.
    /// An invalid frame leaves the child and queue unchanged.
    /// Dropping this endpoint does not interrupt a pipe write. Use shutdown for that.
    #[allow(
        clippy::needless_pass_by_ref_mut,
        reason = "allow only one in-flight send"
    )]
    pub async fn send(&mut self, frame: &[u8]) -> Result<(), StdioError> {
        if frame.len() > self.max_frame_bytes {
            return Err(StdioError::InputTooLarge);
        }
        if frame.contains(&b'\n') {
            return Err(StdioError::InputContainsNewline);
        }
        let permit = self
            .queue
            .reserve()
            .await
            .map_err(|_| StdioError::InputClosed)?;
        permit.send(frame.to_vec());
        Ok(())
    }
}

pub struct StdioOutput(mpsc::Receiver<Vec<u8>>);

impl StdioOutput {
    /// Receive one opaque frame with only its LF terminator removed.
    /// Frames already queued can outlive cancellation. Check the lease before forwarding.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.0.recv().await
    }
}

pub struct StdioLease {
    pub parent: CancellationToken,
    pub deadline: Option<Instant>,
}

impl StdioLease {
    fn ensure_active(&self) -> Result<(), StdioError> {
        if self.parent.is_cancelled() {
            return Err(StdioError::Inactive(StdioExit::Cancelled));
        }
        if self
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            return Err(StdioError::Inactive(StdioExit::Expired));
        }
        Ok(())
    }
}

struct SupervisorStop {
    shutdown: CancellationToken,
    lease: StdioLease,
}

pub struct StdioChild {
    stop: CancellationToken,
    task: JoinHandle<StdioOutcome>,
    result: Option<StdioOutcome>,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pid: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct StdioOutcome {
    pub(crate) result: Result<StdioExit, StdioError>,
    pub(crate) reaped: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StdioStartError {
    BeforeLaunch(StdioError),
    AfterLaunch(StdioError),
}

impl From<StdioError> for StdioStartError {
    fn from(error: StdioError) -> Self {
        Self::BeforeLaunch(error)
    }
}

pub(crate) struct PinnedDirectory {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    file: std::fs::File,
}

impl PinnedDirectory {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn open(root: &Path, relative: &Path) -> Result<Self, StdioError> {
        use rustix::fs::{Mode, OFlags, open, openat};
        use std::path::Component;

        if !root.is_absolute() || relative.as_os_str().is_empty() {
            return Err(StdioError::InvalidLaunch);
        }
        #[cfg(target_os = "linux")]
        let access = OFlags::PATH;
        #[cfg(target_os = "macos")]
        let access = OFlags::RDONLY;
        let flags = access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let invalid = |_| StdioError::InvalidLaunch;
        let mut fd = open("/", flags, Mode::empty()).map_err(invalid)?;
        for component in root
            .components()
            .skip(1)
            .chain(relative.components().filter(|_| relative != Path::new(".")))
        {
            let Component::Normal(name) = component else {
                return Err(StdioError::InvalidLaunch);
            };
            fd = openat(&fd, name, flags, Mode::empty()).map_err(invalid)?;
        }
        Ok(Self { file: fd.into() })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn open(_root: &Path, _relative: &Path) -> Result<Self, StdioError> {
        Err(StdioError::InvalidLaunch)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn spawn_path(&self) -> Result<std::path::PathBuf, StdioError> {
        use std::os::unix::fs::MetadataExt;

        let expected = self
            .file
            .metadata()
            .map_err(|_| StdioError::InvalidLaunch)?;
        #[cfg(target_os = "linux")]
        let path = {
            use std::os::fd::AsRawFd;
            format!("/proc/self/fd/{}", self.file.as_raw_fd())
        };
        #[cfg(target_os = "macos")]
        let path = format!("/.vol/{}/{}", expected.dev(), expected.ino());
        // These kernel paths identify the held directory, not its mutable workspace path.
        let actual = std::fs::metadata(&path).map_err(|_| StdioError::InvalidLaunch)?;
        if !actual.is_dir() || (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
            return Err(StdioError::InvalidLaunch);
        }
        Ok(path.into())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn spawn_path(&self) -> Result<std::path::PathBuf, StdioError> {
        Err(StdioError::InvalidLaunch)
    }
}

impl StdioChild {
    /// Start only a trusted, pre-resolved worker-local executable and arguments in an
    /// independently validated, canonical worker directory. Neither path nor the arguments are
    /// wire input. Requires a supported platform with a working kernel directory-identity path.
    /// The child does not inherit the worker environment; stderr goes to the null device.
    ///
    /// The optional deadline is fixed for this child. For a renewable lease, pass no deadline
    /// and cancel the parent token on expiry. Use a Tokio runtime with I/O and time drivers.
    pub fn spawn(
        executable: &Path,
        args: &[OsString],
        cwd: &Path,
        lease: StdioLease,
        max_frame_bytes: usize,
    ) -> Result<(Self, StdioInput, StdioOutput), StdioError> {
        lease.ensure_active()?;
        let cwd = PinnedDirectory::open(cwd, Path::new("."))?;
        Self::spawn_pinned(executable, args, cwd, lease, max_frame_bytes).map_err(|error| {
            match error {
                StdioStartError::BeforeLaunch(error) | StdioStartError::AfterLaunch(error) => error,
            }
        })
    }

    pub(crate) fn spawn_pinned(
        executable: &Path,
        args: &[OsString],
        cwd: PinnedDirectory,
        lease: StdioLease,
        max_frame_bytes: usize,
    ) -> Result<(Self, StdioInput, StdioOutput), StdioStartError> {
        if !executable.is_absolute() {
            return Err(StdioError::InvalidLaunch.into());
        }
        // Windows Command otherwise permits batch files via cmd.exe.
        #[cfg(windows)]
        if !executable
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            return Err(StdioError::InvalidLaunch.into());
        }
        if !(1..=MAX_FRAME_BYTES).contains(&max_frame_bytes) {
            return Err(StdioError::InvalidFrameLimit.into());
        }
        lease.ensure_active()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| StdioError::RuntimeUnavailable)?;
        let spawn_path = cwd.spawn_path()?;
        let notification = ChildNotification::new()?;
        lease.ensure_active()?;
        // Keep an owning child handle even if async pipe attachment fails after exec.
        let child = Command::new(executable)
            .args(args)
            .current_dir(spawn_path)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| StdioError::Spawn)?;
        drop(cwd);
        let mut child = DirectChild {
            child,
            notification,
            reaped: false,
        };
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        let pid = child.child.id();
        let pipes = child.attach_pipes()?;
        let (input, inbound) = mpsc::channel(QUEUE_DEPTH);
        let (outbound, output) = mpsc::channel(QUEUE_DEPTH);
        let stop = CancellationToken::new();
        let task = runtime.spawn(supervise(
            child,
            pipes,
            inbound,
            outbound,
            SupervisorStop {
                shutdown: stop.clone(),
                lease,
            },
            max_frame_bytes,
        ));
        Ok((
            Self {
                stop,
                task,
                result: None,
                #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
                pid,
            },
            StdioInput {
                queue: input,
                max_frame_bytes,
            },
            StdioOutput(output),
        ))
    }

    /// Wait for supervisor completion and direct-child reaping. This wait is cancel-safe
    /// and repeatable. The caller must drain stdout or request shutdown if stdout backs
    /// up or a descendant holds the pipe open.
    pub async fn join(&mut self) -> Result<StdioExit, StdioError> {
        self.join_outcome().await.result
    }

    pub(crate) async fn join_outcome(&mut self) -> StdioOutcome {
        if let Some(result) = self.result {
            return result;
        }
        let result = (&mut self.task).await.unwrap_or(StdioOutcome {
            result: Err(StdioError::Supervisor),
            reaped: false,
        });
        debug_assert!(result.reaped || result.result.is_err());
        self.result = Some(result);
        result
    }

    /// Stop both pipes and join without waiting for either caller-side queue.
    /// Only the direct child is terminated and reaped, not its descendants.
    /// If the supervisor already finished, return its original result.
    pub async fn shutdown(&mut self) -> Result<StdioExit, StdioError> {
        self.shutdown_outcome().await.result
    }

    pub(crate) async fn shutdown_outcome(&mut self) -> StdioOutcome {
        self.stop.cancel();
        self.join_outcome().await
    }
}

impl Drop for StdioChild {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct ChildNotification {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
}

impl ChildNotification {
    fn new() -> Result<Self, StdioError> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                signal: signal(SignalKind::child()).map_err(|_| StdioError::RuntimeUnavailable)?,
            })
        }
        #[cfg(not(unix))]
        Err(StdioError::RuntimeUnavailable)
    }

    async fn child_changed(&mut self) -> Result<(), StdioError> {
        #[cfg(unix)]
        {
            self.signal.recv().await.ok_or(StdioError::Wait)
        }
        #[cfg(not(unix))]
        {
            std::future::ready(Err(StdioError::Wait)).await
        }
    }
}

struct DirectChild {
    child: Child,
    notification: ChildNotification,
    reaped: bool,
}

impl DirectChild {
    fn attach_pipes(&mut self) -> Result<(ChildStdin, ChildStdout), StdioStartError> {
        let failure = StdioStartError::AfterLaunch(StdioError::MissingPipe);
        let stdin = self.child.stdin.take().ok_or(failure)?;
        let stdout = self.child.stdout.take().ok_or(failure)?;
        let stdin = ChildStdin::from_std(stdin).map_err(|_| failure)?;
        let stdout = ChildStdout::from_std(stdout).map_err(|_| failure)?;
        Ok((stdin, stdout))
    }

    fn try_reap(&mut self) -> Result<bool, StdioError> {
        if !self.reaped {
            self.reaped = self
                .child
                .try_wait()
                .map_err(|_| StdioError::Wait)?
                .is_some();
        }
        Ok(self.reaped)
    }

    async fn wait(&mut self) -> Result<(), StdioError> {
        while !self.try_reap()? {
            self.notification.child_changed().await?;
        }
        Ok(())
    }

    async fn terminate_and_wait(&mut self) -> Result<(), StdioError> {
        if !self.try_reap()? && self.child.kill().is_err() && !self.try_reap()? {
            return Err(StdioError::Terminate);
        }
        self.wait().await
    }
}

impl Drop for DirectChild {
    fn drop(&mut self) {
        // The async supervisor normally reaps; this also covers post-launch setup failure.
        if self.reaped {
            return;
        }
        if self.child.kill().is_ok() {
            drop(self.child.wait());
        } else {
            drop(self.child.try_wait());
        }
    }
}

async fn supervise(
    mut child: DirectChild,
    pipes: (ChildStdin, ChildStdout),
    input: mpsc::Receiver<Vec<u8>>,
    output: mpsc::Sender<Vec<u8>>,
    stop: SupervisorStop,
    max_frame_bytes: usize,
) -> StdioOutcome {
    let result = tokio::select! {
        biased;
        () = stop.shutdown.cancelled() => Ok(StdioExit::Shutdown),
        () = stop.lease.parent.cancelled() => Ok(StdioExit::Cancelled),
        () = wait_for_deadline(stop.lease.deadline) => Ok(StdioExit::Expired),
        () = output.closed() => Ok(StdioExit::OutputDropped),
        result = drive_pipes(&mut child, pipes, input, &output, max_frame_bytes) => result,
    };
    // The select drops both pipe futures before cleanup, even under backpressure.
    if !child.reaped
        && let Err(error) = child.terminate_and_wait().await
    {
        return StdioOutcome {
            result: Err(error),
            reaped: false,
        };
    }
    StdioOutcome {
        result,
        reaped: true,
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        sleep_until(deadline).await;
    } else {
        pending::<()>().await;
    }
}

async fn drive_pipes(
    child: &mut DirectChild,
    (stdin, stdout): (ChildStdin, ChildStdout),
    input: mpsc::Receiver<Vec<u8>>,
    output: &mpsc::Sender<Vec<u8>>,
    max_frame_bytes: usize,
) -> Result<StdioExit, StdioError> {
    let read = read_stdout(stdout, output, max_frame_bytes);
    tokio::pin!(read);
    tokio::select! {
        biased;
        result = &mut read => result,
        status = child.wait() => {
            status?;
            // The child is already reaped. Preserve complete final stdout frames.
            read.await
        }
        result = write_stdin(stdin, input) => result,
    }
}

async fn write_stdin(
    mut stdin: ChildStdin,
    mut input: mpsc::Receiver<Vec<u8>>,
) -> Result<StdioExit, StdioError> {
    while let Some(frame) = input.recv().await {
        stdin
            .write_all(&frame)
            .await
            .map_err(|_| StdioError::Write)?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|_| StdioError::Write)?;
    }
    Ok(StdioExit::InputDropped)
}

async fn read_stdout(
    stdout: ChildStdout,
    output: &mpsc::Sender<Vec<u8>>,
    max_frame_bytes: usize,
) -> Result<StdioExit, StdioError> {
    let mut stdout = BufReader::new(stdout);
    let mut frame = Vec::new();
    loop {
        let bytes = stdout.fill_buf().await.map_err(|_| StdioError::Read)?;
        if bytes.is_empty() {
            return if frame.is_empty() {
                Ok(StdioExit::StdoutClosed)
            } else {
                Err(StdioError::UnterminatedOutput)
            };
        }
        let newline = bytes.iter().position(|&byte| byte == b'\n');
        let payload = newline.unwrap_or(bytes.len());
        if payload > max_frame_bytes - frame.len() {
            return Err(StdioError::OutputTooLarge);
        }
        frame.extend_from_slice(&bytes[..payload]);
        stdout.consume(payload + usize::from(newline.is_some()));
        if newline.is_some() && output.send(std::mem::take(&mut frame)).await.is_err() {
            return Ok(StdioExit::OutputDropped);
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
