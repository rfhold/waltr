use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use super::{ShellConfig, ShellInput, format_process_output, shell_error};
use crate::{Result, ToolProgressSink};

const OUTPUT_READ_BUFFER_BYTES: usize = 8192;

impl ShellConfig {
    /// Execute a shell command using the configured root and limits.
    ///
    /// # Errors
    ///
    /// Returns an error if command input is invalid or the process cannot be spawned.
    pub fn execute(&self, input: &ShellInput) -> Result<String> {
        self.execute_with_progress(input, &ToolProgressSink::noop())
    }

    /// Execute a shell command and stream stdout/stderr as generic progress.
    ///
    /// # Errors
    ///
    /// Returns an error if command input is invalid, the process cannot be spawned, output cannot
    /// be read, or progress cannot be emitted.
    pub fn execute_with_progress(
        &self,
        input: &ShellInput,
        progress: &ToolProgressSink,
    ) -> Result<String> {
        let workdir = self.resolved_workdir(input.workdir.as_deref());
        let timeout = input
            .timeout
            .map_or(self.default_timeout, Duration::from_secs);
        let mut child = Command::new("/bin/bash")
            .arg("-lc")
            .arg(&input.command)
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| shell_error("failed to capture shell stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| shell_error("failed to capture shell stderr"))?;
        let stdout_progress = progress.clone();
        let stderr_progress = progress.clone();
        let stdout_reader = thread::spawn(move || read_output(stdout, stdout_progress, false));
        let stderr_reader = thread::spawn(move || read_output(stderr, stderr_progress, true));

        let start = Instant::now();
        let (exit_code, timed_out) = loop {
            if let Some(status) = child.try_wait()? {
                break (status.code(), false);
            }

            if start.elapsed() >= timeout {
                drop(child.kill());
                let status = child.wait()?;
                break (status.code(), true);
            }

            thread::sleep(Duration::from_millis(10));
        };
        let stdout = join_output(stdout_reader)?;
        let stderr = join_output(stderr_reader)?;

        Ok(format_process_output(
            exit_code,
            &stdout,
            &stderr,
            self.max_output_bytes,
            timed_out,
        ))
    }
}

fn read_output(
    mut reader: impl Read,
    progress: ToolProgressSink,
    is_error: bool,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; OUTPUT_READ_BUFFER_BYTES];
    let mut progress_error = None;
    loop {
        let bytes = reader.read(&mut buffer)?;
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

fn join_output(handle: thread::JoinHandle<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| shell_error("shell output reader panicked"))?
}
