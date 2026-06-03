use std::fmt::Write as _;

pub(super) fn format_process_output(
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    max_output_bytes: usize,
    timed_out: bool,
) -> String {
    let mut output = String::new();
    if timed_out {
        output.push_str("Timed out: true\n");
    }
    writeln!(
        output,
        "Exit code: {}\n",
        exit_code.map_or_else(|| "signal".to_owned(), |code| code.to_string())
    )
    .expect("write process output");
    output.push_str(&String::from_utf8_lossy(stdout));
    output.push_str(&String::from_utf8_lossy(stderr));
    truncate_output(output, max_output_bytes)
}

fn truncate_output(mut output: String, max_output_bytes: usize) -> String {
    if output.len() <= max_output_bytes {
        return output;
    }
    output.truncate(max_output_bytes);
    output.push_str("\n[output truncated]");
    output
}
