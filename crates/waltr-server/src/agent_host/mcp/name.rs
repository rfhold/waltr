pub(in crate::agent_host::mcp) fn direct_tool_name(server: &str, tool: &str) -> String {
    format!(
        "{}__{}",
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visible_aliases_use_the_server_namespace_without_a_mandatory_product_prefix() {
        assert_eq!(
            direct_tool_name("search", "lookup.items"),
            "search__lookup_items"
        );
        assert_ne!(
            direct_tool_name("one", "lookup"),
            direct_tool_name("two", "lookup")
        );
    }
}
