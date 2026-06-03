use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use super::{ShellConfig, ShellInput, format_process_output};
use crate::Result;

impl ShellConfig {
    /// Execute a shell command using the configured root and limits.
    ///
    /// # Errors
    ///
    /// Returns an error if command input is invalid or the process cannot be spawned.
    pub fn execute(&self, input: &ShellInput) -> Result<String> {
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

        let start = Instant::now();
        loop {
            if child.try_wait()?.is_some() {
                let output = child.wait_with_output()?;
                return Ok(format_process_output(
                    output.status.code(),
                    &output.stdout,
                    &output.stderr,
                    self.max_output_bytes,
                    false,
                ));
            }

            if start.elapsed() >= timeout {
                drop(child.kill());
                let output = child.wait_with_output()?;
                return Ok(format_process_output(
                    output.status.code(),
                    &output.stdout,
                    &output.stderr,
                    self.max_output_bytes,
                    true,
                ));
            }

            thread::sleep(Duration::from_millis(10));
        }
    }
}
