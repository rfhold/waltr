use futures_util::{Stream, StreamExt, stream as futures_stream};
use tracing::Instrument as _;

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
            provider.prompt_cache,
            &request,
        ));
    for (name, value) in &provider.headers {
        request_builder = request_builder.header(name.as_str(), value);
    }
    let span = tracing::info_span!(
        "http.client.request",
        http_request_method = "POST",
        server_address = %provider.messages_url,
        provider = PROVIDER,
        model = %provider.model,
        http_response_status_code = tracing::field::Empty,
    );
    let response = request_builder.send().instrument(span.clone()).await?;

    let status = response.status();
    span.record("http_response_status_code", status.as_u16());
    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), "provider http request failed");
        let text = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
        return Err(LlmError::Api {
            provider: PROVIDER,
            message: format!("{status}: {text}"),
        });
    }
    span.in_scope(|| tracing::debug!(status = status.as_u16(), "provider http stream opened"));
    let stream_span = tracing::info_span!(
        "provider.stream_body",
        provider = PROVIDER,
        model = %provider.model,
        terminal_state = tracing::field::Empty,
        chunk_count = tracing::field::Empty,
        event_count = tracing::field::Empty,
    );

    Ok(completion_stream(
        response.bytes_stream(),
        AnthropicParser::new(
            super::ANTHROPIC_MESSAGES_NAME,
            &provider.messages_url,
            &provider.model,
        ),
        stream_span,
    ))
}

fn completion_stream<S, B>(
    bytes_stream: S,
    parser: AnthropicParser,
    stream_span: tracing::Span,
) -> BoxCompletionStream
where
    S: Stream<Item = std::result::Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    // Parser progress is intentionally not persisted: crash recovery retries
    // the whole turn from the journaled request record.
    let bytes_stream = Box::pin(bytes_stream);
    Box::pin(futures_stream::unfold(
        (
            bytes_stream,
            Vec::new(),
            parser,
            false,
            stream_span,
            0_u64,
            0_u64,
        ),
        |(
            mut bytes_stream,
            mut buffer,
            mut parser,
            done,
            stream_span,
            mut chunk_count,
            mut event_count,
        )| async move {
            if done {
                return None;
            }

            loop {
                if let Some(result) = next_event(&mut buffer, &mut parser) {
                    match result {
                        Ok(Some(event)) => {
                            let done = matches!(event, CompletionEvent::Finished);
                            event_count += 1;
                            if done {
                                record_stream_end(
                                    &stream_span,
                                    "finished",
                                    chunk_count,
                                    event_count,
                                );
                            }
                            return Some((
                                Ok(event),
                                (
                                    bytes_stream,
                                    buffer,
                                    parser,
                                    done,
                                    stream_span,
                                    chunk_count,
                                    event_count,
                                ),
                            ));
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            record_stream_error(&stream_span, &error, chunk_count, event_count);
                            return Some((
                                Err(error),
                                (
                                    bytes_stream,
                                    buffer,
                                    parser,
                                    true,
                                    stream_span,
                                    chunk_count,
                                    event_count,
                                ),
                            ));
                        }
                    }
                }

                match bytes_stream.next().await {
                    Some(Ok(bytes)) => {
                        chunk_count += 1;
                        buffer.extend_from_slice(bytes.as_ref());
                    }
                    Some(Err(error)) => {
                        let error = error.into();
                        record_stream_error(&stream_span, &error, chunk_count, event_count);
                        return Some((
                            Err(error),
                            (
                                bytes_stream,
                                buffer,
                                parser,
                                true,
                                stream_span,
                                chunk_count,
                                event_count,
                            ),
                        ));
                    }
                    None => {
                        if buffer.is_empty() {
                            record_stream_end(&stream_span, "eof", chunk_count, event_count);
                            return None;
                        }

                        let result = parse_sse_event(std::mem::take(&mut buffer), &mut parser);
                        match result {
                            Ok(Some(event)) => {
                                let done = matches!(event, CompletionEvent::Finished);
                                event_count += 1;
                                if done {
                                    record_stream_end(
                                        &stream_span,
                                        "finished",
                                        chunk_count,
                                        event_count,
                                    );
                                }
                                return Some((
                                    Ok(event),
                                    (
                                        bytes_stream,
                                        buffer,
                                        parser,
                                        done,
                                        stream_span,
                                        chunk_count,
                                        event_count,
                                    ),
                                ));
                            }
                            Ok(None) => {
                                record_stream_end(&stream_span, "eof", chunk_count, event_count);
                                return None;
                            }
                            Err(error) => {
                                record_stream_error(&stream_span, &error, chunk_count, event_count);
                                return Some((
                                    Err(error),
                                    (
                                        bytes_stream,
                                        buffer,
                                        parser,
                                        true,
                                        stream_span,
                                        chunk_count,
                                        event_count,
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        },
    ))
}

fn record_stream_end(
    stream_span: &tracing::Span,
    terminal_state: &'static str,
    chunk_count: u64,
    event_count: u64,
) {
    stream_span.record("terminal_state", terminal_state);
    stream_span.record("chunk_count", chunk_count);
    stream_span.record("event_count", event_count);
    stream_span.in_scope(|| {
        tracing::info!(
            chunk_count,
            event_count,
            terminal_state,
            "provider stream ended"
        )
    });
}

fn record_stream_error(
    stream_span: &tracing::Span,
    error: &LlmError,
    chunk_count: u64,
    event_count: u64,
) {
    stream_span.record("terminal_state", "error");
    stream_span.record("chunk_count", chunk_count);
    stream_span.record("event_count", event_count);
    stream_span
        .in_scope(|| tracing::warn!(%error, chunk_count, event_count, "provider stream failed"));
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
