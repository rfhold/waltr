use std::{future::Future, time::Duration};

use tokio::time::timeout;

use super::*;

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    timeout(Duration::from_secs(5), future).await.unwrap()
}

fn spawn(
    executable: &str,
    args: &[&str],
    cwd: &Path,
    lease: StdioLease,
    max_frame_bytes: usize,
) -> (StdioChild, StdioInput, StdioOutput) {
    let args: Vec<_> = args.iter().map(OsString::from).collect();
    StdioChild::spawn(
        Path::new(executable),
        &args,
        &cwd.canonicalize().unwrap(),
        lease,
        max_frame_bytes,
    )
    .unwrap()
}

const fn lease(parent: CancellationToken, deadline: Option<Instant>) -> StdioLease {
    StdioLease { parent, deadline }
}

async fn pid_present(pid: u32) -> bool {
    // A zombie still has a process table entry.
    let status = bounded(
        tokio::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await
    .unwrap();
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        code => panic!("unexpected ps exit code: {code:?}"),
    }
}

async fn assert_reaped(pid: u32) {
    assert!(!pid_present(pid).await, "direct child PID still exists");
}

#[tokio::test]
async fn child_uses_pinned_directory_identity_after_workspace_path_is_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let live = root.join("work");
    let moved = root.join("moved");
    std::fs::create_dir(&live).unwrap();
    std::fs::write(live.join("marker"), b"original\n").unwrap();
    let pinned = PinnedDirectory::open(&root, Path::new("work")).unwrap();

    std::fs::rename(&live, &moved).unwrap();
    std::fs::create_dir(&live).unwrap();
    std::fs::write(live.join("marker"), b"replacement\n").unwrap();
    let (mut child, _input, mut output) = StdioChild::spawn_pinned(
        Path::new("/bin/cat"),
        &[OsString::from("marker")],
        pinned,
        lease(CancellationToken::new(), None),
        16,
    )
    .unwrap();

    assert_eq!(
        bounded(output.recv()).await.as_deref(),
        Some(b"original".as_slice())
    );
    assert_eq!(bounded(child.join()).await, Ok(StdioExit::StdoutClosed));
    assert_reaped(child.pid).await;
}

#[test]
fn pin_only_opens_export_relative_directories_without_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("known")).unwrap();
    std::os::unix::fs::symlink("known", root.join("alias")).unwrap();
    assert!(PinnedDirectory::open(&root, Path::new(".")).is_ok());
    assert!(PinnedDirectory::open(&root, Path::new("known")).is_ok());
    for rejected in ["/", "", "../", "alias", "known/../known", "./known"] {
        assert!(
            matches!(
                PinnedDirectory::open(&root, Path::new(rejected)),
                Err(StdioError::InvalidLaunch)
            ),
            "{rejected}"
        );
    }
}

#[tokio::test]
async fn post_launch_pipe_setup_failure_keeps_child_owned_until_reaped() {
    let notification = ChildNotification::new().unwrap();
    let child = Command::new("/bin/sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut child = DirectChild {
        child,
        notification,
        reaped: false,
    };
    assert!(matches!(
        child.attach_pipes(),
        Err(StdioStartError::AfterLaunch(StdioError::MissingPipe))
    ));
    drop(child);
    assert_reaped(pid).await;
}

#[tokio::test]
async fn invalid_executable_format_respects_the_platform_launch_contract() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let program = root.join("text");
    std::fs::write(&program, b"printf 'not a program\\n'\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    match StdioChild::spawn_pinned(
        &program,
        &[],
        PinnedDirectory::open(&root, Path::new(".")).unwrap(),
        lease(CancellationToken::new(), None),
        32,
    ) {
        Err(StdioStartError::BeforeLaunch(StdioError::Spawn)) => {}
        Ok((mut child, _input, mut output)) => {
            assert!(cfg!(target_os = "macos"), "unexpected shell fallback");
            assert_eq!(
                bounded(output.recv()).await.as_deref(),
                Some(b"not a program".as_slice())
            );
            assert!(bounded(child.join_outcome()).await.reaped);
        }
        Err(error) => panic!("unexpected launch error: {error:?}"),
    }
}

#[tokio::test]
async fn round_trip_uses_worker_directory_and_preserves_frame_bytes() {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("fixture"), b"cwd-marker\n").unwrap();
    let (mut child, mut input, mut output) = spawn(
        "/bin/cat",
        &["fixture", "-"],
        cwd.path(),
        lease(CancellationToken::new(), None),
        16,
    );
    assert_eq!(
        bounded(output.recv()).await.as_deref(),
        Some(b"cwd-marker".as_slice())
    );
    assert_eq!(
        input.send(b"12345678901234567").await,
        Err(StdioError::InputTooLarge)
    );
    assert_eq!(
        input.send(b"two\nlines").await,
        Err(StdioError::InputContainsNewline)
    );
    let frame = b"opaque\xff bytes!\r!";
    assert_eq!(frame.len(), 16);
    bounded(input.send(frame)).await.unwrap();
    assert_eq!(
        bounded(output.recv()).await.as_deref(),
        Some(frame.as_slice())
    );
    assert_eq!(bounded(child.shutdown()).await, Ok(StdioExit::Shutdown));
    assert_eq!(child.join().await, Ok(StdioExit::Shutdown));
    assert_eq!(input.send(b"closed").await, Err(StdioError::InputClosed));
    assert_eq!(bounded(output.recv()).await, None);
    assert_reaped(child.pid).await;
}

#[tokio::test]
async fn natural_exit_reaps_before_a_backed_up_final_frame_drains() {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(
        cwd.path().join("fixture"),
        b"last\n".repeat(QUEUE_DEPTH + 1),
    )
    .unwrap();
    let (mut child, _input, mut output) = spawn(
        "/bin/cat",
        &["fixture"],
        cwd.path(),
        lease(CancellationToken::new(), None),
        4,
    );
    bounded(async {
        while pid_present(child.pid).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    for _ in 0..=QUEUE_DEPTH {
        assert_eq!(
            bounded(output.recv()).await.as_deref(),
            Some(b"last".as_slice())
        );
    }
    assert_eq!(bounded(output.recv()).await, None);
    assert_eq!(bounded(child.join()).await, Ok(StdioExit::StdoutClosed));
    assert_reaped(child.pid).await;
}

#[tokio::test]
async fn oversized_and_unterminated_stdout_are_terminal() {
    let cwd = tempfile::tempdir().unwrap();
    for (bytes, max_frame_bytes, error) in [
        (vec![b'x'; 32 * 1024], 16 * 1024, StdioError::OutputTooLarge),
        (b"partial".to_vec(), 8, StdioError::UnterminatedOutput),
    ] {
        std::fs::write(cwd.path().join("fixture"), bytes).unwrap();
        let (mut child, _input, mut output) = spawn(
            "/bin/cat",
            &["fixture"],
            cwd.path(),
            lease(CancellationToken::new(), None),
            max_frame_bytes,
        );
        let outcome = bounded(child.join_outcome()).await;
        assert_eq!(outcome.result, Err(error));
        assert!(outcome.reaped);
        assert_eq!(bounded(output.recv()).await, None);
        assert_reaped(child.pid).await;
    }
}

#[tokio::test]
async fn parent_cancellation_reaps_child_despite_full_stdout_queue() {
    let cwd = tempfile::tempdir().unwrap();
    let parent = CancellationToken::new();
    let (mut child, _input, output) = spawn(
        "/usr/bin/yes",
        &[],
        cwd.path(),
        lease(parent.clone(), None),
        16,
    );
    bounded(async {
        while output.0.len() != QUEUE_DEPTH {
            tokio::task::yield_now().await;
        }
    })
    .await;
    parent.cancel();
    assert_eq!(bounded(child.join()).await, Ok(StdioExit::Cancelled));
    assert_reaped(child.pid).await;
}

#[tokio::test]
async fn explicit_shutdown_reaps_child_despite_blocked_stdin() {
    let cwd = tempfile::tempdir().unwrap();
    let (mut child, mut input, _output) = spawn(
        "/bin/sleep",
        &["60"],
        cwd.path(),
        lease(CancellationToken::new(), None),
        MAX_FRAME_BYTES,
    );
    let frame = vec![b'x'; MAX_FRAME_BYTES];
    // Fill one pipe write and the entire input queue. The fixture never reads stdin.
    for _ in 0..=QUEUE_DEPTH {
        bounded(input.send(&frame)).await.unwrap();
    }
    assert_eq!(input.queue.capacity(), 0);
    assert!(
        timeout(Duration::from_millis(30), input.send(&frame))
            .await
            .is_err()
    );
    assert_eq!(bounded(child.shutdown()).await, Ok(StdioExit::Shutdown));
    assert_eq!(input.send(b"closed").await, Err(StdioError::InputClosed));
    assert_reaped(child.pid).await;
}

#[tokio::test]
async fn fixed_deadline_expires_and_inactive_leases_do_not_spawn() {
    let cwd = tempfile::tempdir().unwrap();
    let parent = CancellationToken::new();
    parent.cancel();
    let launch_missing = |parent, deadline| {
        StdioChild::spawn(
            &cwd.path().join("no-such-executable"),
            &[],
            cwd.path(),
            lease(parent, deadline),
            16,
        )
        .err()
    };
    assert_eq!(
        launch_missing(parent, None),
        Some(StdioError::Inactive(StdioExit::Cancelled))
    );
    assert_eq!(
        launch_missing(CancellationToken::new(), Some(Instant::now())),
        Some(StdioError::Inactive(StdioExit::Expired))
    );
    let (mut child, _input, _output) = spawn(
        "/bin/sleep",
        &["60"],
        cwd.path(),
        lease(
            CancellationToken::new(),
            Some(Instant::now() + Duration::from_secs(1)),
        ),
        16,
    );
    assert_eq!(bounded(child.join()).await, Ok(StdioExit::Expired));
    assert_reaped(child.pid).await;
}
