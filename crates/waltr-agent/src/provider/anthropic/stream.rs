use futures_util::{Stream, StreamExt, stream as futures_stream};

use crate::{BoxCompletionStream, CompletionEvent, CompletionRequest, LlmError, Result};

use super::{AnthropicMessages, PROVIDER, parser::AnthropicParser, request::messages_request};

pub(super) async fn send_request(
    provider: &AnthropicMessages,
    request: CompletionRequest<'_>,
) -> Result<BoxCompletionStream> {
    // Must stay in lockstep with `LlmStream::project_request`: the journaled
    // projection is the exact payload sent here.
    let request = request.for_provider_identity(
        super::ANTHROPIC_MESSAGES_NAME,
        &provider.messages_url,
        &provider.model,
    );
    let mut request_builder = provider
        .client
        .post(&provider.messages_url)
        .header("anthropic-version", "2023-06-01")
        .json(&messages_request(
            &provider.model,
            provider.max_tokens,
            provider.thinking.as_ref(),
            &request,
        ));
    for (name, value) in &provider.headers {
        request_builder = request_builder.header(*name, value);
    }
    let response = request_builder.send().await?;

    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
        return Err(LlmError::Api {
            provider: PROVIDER,
            message: format!("{status}: {text}"),
        });
    }

    Ok(completion_stream(
        response.bytes_stream(),
        AnthropicParser::new(
            super::ANTHROPIC_MESSAGES_NAME,
            &provider.messages_url,
            &provider.model,
        ),
    ))
}

fn completion_stream<S, B>(bytes_stream: S, parser: AnthropicParser) -> BoxCompletionStream
where
    S: Stream<Item = std::result::Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    // Parser progress is intentionally not persisted: crash recovery retries
    // the whole turn from the journaled request record.
    let bytes_stream = Box::pin(bytes_stream);
    Box::pin(futures_stream::unfold(
        (bytes_stream, Vec::new(), parser, false),
        |(mut bytes_stream, mut buffer, mut parser, done)| async move {
            if done {
                return None;
            }

            loop {
                if let Some(result) = next_event(&mut buffer, &mut parser) {
                    match result {
                        Ok(Some(event)) => {
                            let done = matches!(event, CompletionEvent::Finished);
                            return Some((Ok(event), (bytes_stream, buffer, parser, done)));
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            return Some((Err(error), (bytes_stream, buffer, parser, true)));
                        }
                    }
                }

                match bytes_stream.next().await {
                    Some(Ok(bytes)) => buffer.extend_from_slice(bytes.as_ref()),
                    Some(Err(error)) => {
                        return Some((Err(error.into()), (bytes_stream, buffer, parser, true)));
                    }
                    None => {
                        if buffer.is_empty() {
                            return None;
                        }

                        let result = parse_sse_event(std::mem::take(&mut buffer), &mut parser);
                        match result {
                            Ok(Some(event)) => {
                                let done = matches!(event, CompletionEvent::Finished);
                                return Some((Ok(event), (bytes_stream, buffer, parser, done)));
                            }
                            Ok(None) => return None,
                            Err(error) => {
                                return Some((Err(error), (bytes_stream, buffer, parser, true)));
                            }
                        }
                    }
                }
            }
        },
    ))
}

fn next_event(
    buffer: &mut Vec<u8>,
    parser: &mut AnthropicParser,
) -> Option<Result<Option<CompletionEvent>>> {
    let (index, delimiter_len) = event_delimiter(buffer)?;
    let mut event = buffer.split_off(index + delimiter_len);
    std::mem::swap(buffer, &mut event);
    event.truncate(index);
    Some(parse_sse_event(event, parser))
}

fn event_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, 2))
        })
}

fn parse_sse_event(
    event: Vec<u8>,
    parser: &mut AnthropicParser,
) -> Result<Option<CompletionEvent>> {
    let event = String::from_utf8(event).map_err(|error| LlmError::Protocol {
        provider: PROVIDER,
        message: format!("invalid UTF-8 SSE event: {error}"),
    })?;

    let data = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");

    if data.is_empty() {
        return Ok(None);
    }

    parser.map_event(&serde_json::from_str(&data)?)
}
