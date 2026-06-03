//! Filesystem output formatting helpers.

pub(super) fn truncate_line(line: &str, max_length: usize) -> String {
    if line.len() <= max_length {
        return line.to_owned();
    }

    format!("{}...", &line[..max_length])
}
