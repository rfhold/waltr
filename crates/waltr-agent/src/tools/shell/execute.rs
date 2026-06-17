use std::{process::Stdio, time::Duration};

use super::{ShellConfig, ShellInput, format_process_output, shell_error};
use crate::{Result, ToolProgressSink};
use tokio::{io::AsyncReadExt, process::Command as TokioCommand, time};
use tokio_util::sync::CancellationToken;

const OUTPUT_READ_BUFFER_BYTES: usize = 8192;

impl ShellConfig {
    /// Execute a shell command with progress and cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error if command input is invalid, the process cannot be spawned, output cannot
    /// be read, or progress cannot be emitted.
    pub async fn execute_with_progress_and_cancellation(
        &self,
        input: &ShellInput,
        progress: ToolProgressSink,
        cancellation_token: CancellationToken,
    ) -> Result<String> {
        if cancellation_token.is_cancelled() {
            return Err(shell_error("shell command canceled"));
        }
        let workdir = self.resolved_workdir(input.workdir.as_deref());
        let timeout = input
            .timeout
            .map_or(self.default_timeout, Duration::from_secs);
        let command = cancellation_wrapped_command(&input.command);
        if cancellation_token.is_cancelled() {
            return Err(shell_error("shell command canceled"));
        }
        let mut child = TokioCommand::new("/bin/bash")
            .arg("-lc")
            .arg(command)
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let child_id = child
            .id()
            .ok_or_else(|| shell_error("failed to identify shell process"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| shell_error("failed to capture shell stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| shell_error("failed to capture shell stderr"))?;
        let stdout_reader = tokio::spawn(read_output_async(stdout, progress.clone(), false));
        let stderr_reader = tokio::spawn(read_output_async(stderr, progress, true));

        let (exit_code, timed_out) = tokio::select! {
            biased;

            () = cancellation_token.cancelled() => {
                terminate_shell(child_id).await;
                let status = child.wait().await?;
                (status.code(), false)
            }
            () = time::sleep(timeout) => {
                terminate_shell(child_id).await;
                let status = child.wait().await?;
                (status.code(), true)
            }
            status = child.wait() => {
                (status?.code(), false)
            }
        };
        let stdout = join_output_async(stdout_reader).await?;
        let stderr = join_output_async(stderr_reader).await?;

        Ok(format_process_output(
            exit_code,
            &stdout,
            &stderr,
            self.max_output_bytes,
            timed_out,
        ))
    }
}

fn cancellation_wrapped_command(command: &str) -> String {
    format!("trap 'jobs -pr | xargs -r kill -TERM; exit 143' TERM INT; {command}")
}

async fn terminate_shell(pid: u32) {
    let pid = pid.to_string();
    drop(
        TokioCommand::new("kill")
            .arg("-TERM")
            .arg(&pid)
            .status()
            .await,
    );
    time::sleep(Duration::from_millis(100)).await;
    drop(
        TokioCommand::new("kill")
            .arg("-KILL")
            .arg(&pid)
            .status()
            .await,
    );
}

async fn read_output_async(
    mut reader: impl AsyncReadExt + Unpin,
    progress: ToolProgressSink,
    is_error: bool,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; OUTPUT_READ_BUFFER_BYTES];
    let mut progress_error = None;
    loop {
        let bytes = reader.read(&mut buffer).await?;
        if bytes == 0 {
            break;
        }
        let chunk = &buffer[..bytes];
        output.extend_from_slice(chunk);
        let delta = String::from_utf8_lossy(chunk).into_owned();
        let result = if is_error {
            progress.error(delta)
        } else {
            progress.info(delta)
        };
        if let Err(error) = result
            && progress_error.is_none()
        {
            progress_error = Some(error);
        }
    }
    if let Some(error) = progress_error {
        Err(error)
    } else {
        Ok(output)
    }
}

async fn join_output_async(handle: tokio::task::JoinHandle<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    handle
        .await
        .map_err(|error| shell_error(format!("shell output reader task failed: {error}")))?
}
