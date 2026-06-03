pub(in crate::mcp) fn direct_tool_name(server: &str, tool: &str) -> String {
    format!(
        "mcp_{}_{}",
        sanitize_tool_part(server),
        sanitize_tool_part(tool)
    )
}

fn sanitize_tool_part(part: &str) -> String {
    part.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}
