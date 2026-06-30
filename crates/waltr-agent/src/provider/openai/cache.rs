use serde_json::json;
use sha2::{Digest, Sha256};

use crate::CompletionRequest;

pub(super) fn prompt_cache_key(model: &str, request: &CompletionRequest<'_>) -> String {
    let prefix = json!({
        "model": model,
        "system_prompt": request.system_prompt,
        "tools": request.tools,
    });
    let digest = Sha256::digest(serde_json::to_vec(&prefix).expect("cache key prefix serializes"));
    format!("waltr-{}", hex_prefix(&digest, 16))
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(len);
    for byte in bytes {
        if output.len() == len {
            break;
        }
        output.push(HEX[(byte >> 4) as usize] as char);
        if output.len() == len {
            break;
        }
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{CompletionRequest, ToolDefinition};

    use super::prompt_cache_key;

    #[test]
    fn prompt_cache_key_ignores_current_prompt() {
        let mut first = CompletionRequest::new("first");
        first.system_prompt = Some("system");
        first.tools = vec![ToolDefinition::new(
            "lookup",
            "lookup",
            json!({ "type": "object" }),
        )];
        let mut second = first.clone();
        second.prompt = "second";

        assert_eq!(
            prompt_cache_key("gpt-test", &first),
            prompt_cache_key("gpt-test", &second)
        );
    }
}
