//! Template placeholder styling shared by session composer and timeline text.

use ratatui::{style::Style, text::Span};

pub(super) fn template_inline_spans(
    text: &str,
    base_style: Style,
    token_style: Style,
    error_style: Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut remainder = text;

    while let Some(start) = next_template_marker(remainder) {
        push_resource_token_spans(&mut spans, &remainder[..start], base_style, token_style);
        let marker = &remainder[start..];

        if marker.starts_with("<template-error>") {
            let close = "</template-error>";
            let body_start = "<template-error>".len();
            if let Some(body_end) = marker.find(close) {
                spans.push(Span::styled(
                    decode_xml_entities(&marker[body_start..body_end]),
                    error_style,
                ));
                remainder = &marker[body_end + close.len()..];
                continue;
            }
        }

        if marker.starts_with("<template-result") {
            let close = "</template-result>";
            if let Some(open_end) = marker.find('>') {
                let open_tag = &marker[..=open_end];
                if let Some(body_end) = marker.find(close) {
                    let body = &marker[open_end + 1..body_end];
                    if let Some(reference) = template_result_reference(open_tag, body) {
                        push_resource_token_spans(&mut spans, &reference, base_style, token_style);
                    } else {
                        push_resource_token_spans(
                            &mut spans,
                            &marker[..body_end + close.len()],
                            base_style,
                            token_style,
                        );
                    }
                    remainder = &marker[body_end + close.len()..];
                    continue;
                }
            }
        }

        push_resource_token_spans(&mut spans, marker, base_style, token_style);
        return spans;
    }

    push_resource_token_spans(&mut spans, remainder, base_style, token_style);
    spans
}

pub(super) fn has_template_markers(text: &str) -> bool {
    text.as_bytes()
        .windows(10)
        .any(|window| window == b"<template-")
}

fn next_template_marker(text: &str) -> Option<usize> {
    match (text.find("<template-result"), text.find("<template-error>")) {
        (Some(result), Some(error)) => Some(result.min(error)),
        (Some(result), None) => Some(result),
        (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

fn push_resource_token_spans(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    base_style: Style,
    token_style: Style,
) {
    let mut offset = 0;
    for (start, end) in resource_template_token_ranges(text) {
        if start > offset {
            spans.push(Span::styled(text[offset..start].to_owned(), base_style));
        }
        spans.push(Span::styled(text[start..end].to_owned(), token_style));
        offset = end;
    }
    if offset < text.len() || text.is_empty() {
        spans.push(Span::styled(text[offset..].to_owned(), base_style));
    }
}

fn resource_template_token_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;

    while let Some(relative_start) = text[index..].find('@') {
        let start = index + relative_start;
        if start > 0
            && !text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            index = start + 1;
            continue;
        }

        let Some(end) = resource_template_token_end(text, start) else {
            index = start + 1;
            continue;
        };
        ranges.push((start, end));
        index = end;
    }

    ranges
}

fn resource_template_token_end(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    if rest.starts_with("@\"") {
        return quoted_template_end(text, start + 2);
    }
    if rest.starts_with("@tool(") {
        return balanced_template_end(text, start + "@tool(".len());
    }

    let end = text[start..]
        .char_indices()
        .skip(1)
        .find_map(|(offset, character)| character.is_whitespace().then_some(start + offset))
        .unwrap_or(text.len());
    (end > start + 1).then_some(end)
}

fn quoted_template_end(text: &str, mut index: usize) -> Option<usize> {
    let mut escaped = false;
    while index < text.len() {
        let character = text[index..].chars().next()?;
        index += character.len_utf8();
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return Some(index);
        }
    }
    None
}

fn balanced_template_end(text: &str, mut index: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut string_quote: Option<char> = None;
    let mut escaped = false;

    while index < text.len() {
        let character = text[index..].chars().next()?;
        index += character.len_utf8();

        if let Some(quote) = string_quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote {
                string_quote = None;
            }
            continue;
        }

        match character {
            '"' | '\'' => string_quote = Some(character),
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }

    None
}

fn template_result_reference(open_tag: &str, body: &str) -> Option<String> {
    let kind = template_attr(open_tag, "kind")?;
    match kind.as_str() {
        "file" => template_attr(open_tag, "source").map(|source| file_template_reference(&source)),
        "tool" => {
            let tool = template_attr(open_tag, "tool")?;
            let arguments = template_arguments(body).unwrap_or_else(|| "{}".to_owned());
            Some(format!("@tool({tool}, {arguments})"))
        }
        _ => None,
    }
}

fn template_attr(open_tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = open_tag.find(&needle)? + needle.len();
    let end = open_tag[start..].find('"')?;
    Some(decode_xml_entities(&open_tag[start..start + end]))
}

fn template_arguments(body: &str) -> Option<String> {
    let start_tag = "<arguments>";
    let end_tag = "</arguments>";
    let start = body.find(start_tag)? + start_tag.len();
    let end = body[start..].find(end_tag)?;
    Some(decode_xml_entities(&body[start..start + end]))
}

fn file_template_reference(source: &str) -> String {
    if source
        .chars()
        .all(|character| !character.is_whitespace() && character != '"')
    {
        format!("@{source}")
    } else {
        format!("@\"{}\"", source.replace('"', "\\\""))
    }
}

fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;

    use super::template_inline_spans;

    fn rendered_text(text: &str) -> String {
        template_inline_spans(text, Style::default(), Style::default(), Style::default())
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect()
    }

    #[test]
    fn tool_template_result_uses_spaced_arguments() {
        let text = concat!(
            "Before ",
            "<template-result kind=\"tool\" tool=\"shell\">\n",
            "<arguments>{&quot;command&quot;:&quot;pwd&quot;}</arguments>\n",
            "<content>ok</content>\n",
            "</template-result>",
            " after",
        );

        assert_eq!(
            rendered_text(text),
            "Before @tool(shell, {\"command\":\"pwd\"}) after"
        );
    }

    #[test]
    fn template_result_preserves_surrounding_newlines_once() {
        let text = concat!(
            "Before\n",
            "<template-result kind=\"tool\" tool=\"shell\">\n",
            "<arguments>{&quot;command&quot;:&quot;pwd&quot;}</arguments>\n",
            "<content>ok</content>\n",
            "</template-result>",
            "\nAfter",
        );

        assert_eq!(
            rendered_text(text),
            "Before\n@tool(shell, {\"command\":\"pwd\"})\nAfter"
        );
    }
}
