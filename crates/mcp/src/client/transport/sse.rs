use futures_util::StreamExt;

use crate::{
    Error, Result,
    protocol::{JsonRpcInbound, parse_json_rpc_str},
};

pub(super) const DEFAULT_MAX_SSE_LINE_BYTES: usize = 64 * 1024;
pub(super) const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// One decoded SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// Joined multiline data, when present.
    pub data: Option<String>,
}

/// One SSE JSON-RPC event.
#[derive(Debug)]
pub struct SseJsonRpcEvent {
    /// Parsed JSON-RPC message.
    pub message: JsonRpcInbound,
}

/// Bounded incremental SSE decoder.
#[derive(Debug)]
pub struct SseDecoder {
    utf8_pending: Vec<u8>,
    lines: String,
    data_lines: Vec<String>,
    event_bytes: usize,
    started: bool,
    max_line_bytes: usize,
    max_event_bytes: usize,
}

impl SseDecoder {
    /// Create a decoder with explicit positive line and event limits.
    ///
    /// # Panics
    ///
    /// Panics when either limit is zero.
    #[must_use]
    pub fn with_limits(max_line_bytes: usize, max_event_bytes: usize) -> Self {
        assert!(max_line_bytes > 0, "SSE line limit must be positive");
        assert!(max_event_bytes > 0, "SSE event limit must be positive");
        Self {
            utf8_pending: Vec::new(),
            lines: String::new(),
            data_lines: Vec::new(),
            event_bytes: 0,
            started: false,
            max_line_bytes,
            max_event_bytes,
        }
    }

    /// Feed one response-body chunk and return every complete frame.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid UTF-8 or an event exceeding configured bounds.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>> {
        self.utf8_pending.extend_from_slice(chunk);
        let decoded = match std::str::from_utf8(&self.utf8_pending) {
            Ok(value) => {
                let value = value.to_owned();
                self.utf8_pending.clear();
                value
            }
            Err(error) if error.error_len().is_none() => {
                let valid = self.utf8_pending[..error.valid_up_to()].to_vec();
                self.utf8_pending.drain(..error.valid_up_to());
                String::from_utf8(valid)
                    .map_err(|_| Error::protocol("invalid UTF-8 in SSE response"))?
            }
            Err(_) => return Err(Error::protocol("invalid UTF-8 in SSE response")),
        };
        self.push_text(&decoded, false)
    }

    /// Finish a finite response and dispatch its trailing event.
    ///
    /// # Errors
    ///
    /// Returns an error for incomplete UTF-8 or an event exceeding configured bounds.
    pub fn finish(&mut self) -> Result<Vec<SseFrame>> {
        if !self.utf8_pending.is_empty() {
            return Err(Error::protocol("incomplete UTF-8 in SSE response"));
        }
        self.push_text("", true)
    }

    fn push_text(&mut self, decoded: &str, finish: bool) -> Result<Vec<SseFrame>> {
        self.lines.push_str(decoded);
        if !self.started && !self.lines.is_empty() {
            self.started = true;
            if let Some(without_bom) = self.lines.strip_prefix('\u{feff}') {
                self.lines = without_bom.to_owned();
            }
        }
        let mut frames = Vec::new();
        loop {
            let bytes = self.lines.as_bytes();
            let delimiter = bytes.iter().position(|byte| matches!(byte, b'\r' | b'\n'));
            let Some(index) = delimiter else {
                break;
            };
            if bytes[index] == b'\r' && index + 1 == bytes.len() && !finish {
                break;
            }
            let consumed = if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                index + 2
            } else {
                index + 1
            };
            let line = self.lines[..index].to_owned();
            self.lines.drain(..consumed);
            self.process_line(&line, &mut frames)?;
        }
        if self.lines.len() > self.max_line_bytes {
            return Err(Error::protocol("SSE line exceeds the configured limit"));
        }
        if finish {
            if self.lines.ends_with('\r') {
                self.lines.pop();
            }
            if !self.lines.is_empty() {
                let line = std::mem::take(&mut self.lines);
                self.process_line(&line, &mut frames)?;
            }
            self.dispatch(&mut frames);
        }
        Ok(frames)
    }

    fn process_line(&mut self, line: &str, frames: &mut Vec<SseFrame>) -> Result<()> {
        if line.len() > self.max_line_bytes {
            return Err(Error::protocol("SSE line exceeds the configured limit"));
        }
        if line.is_empty() {
            self.dispatch(frames);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        if field == "data" {
            self.event_bytes = self
                .event_bytes
                .checked_add(value.len() + usize::from(!self.data_lines.is_empty()))
                .filter(|bytes| *bytes <= self.max_event_bytes)
                .ok_or_else(|| Error::protocol("SSE event exceeds the configured limit"))?;
            self.data_lines.push(value.to_owned());
        }
        Ok(())
    }

    fn dispatch(&mut self, frames: &mut Vec<SseFrame>) {
        if self.data_lines.is_empty() {
            return;
        }
        frames.push(SseFrame {
            data: (!self.data_lines.is_empty()).then(|| self.data_lines.join("\n")),
        });
        self.data_lines.clear();
        self.event_bytes = 0;
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::with_limits(DEFAULT_MAX_SSE_LINE_BYTES, DEFAULT_MAX_MESSAGE_BYTES)
    }
}

/// Parse a finite Streamable HTTP SSE response.
///
/// # Errors
///
/// Returns an error when the HTTP body, SSE framing, or JSON-RPC payload is invalid.
pub async fn parse_sse_response(response: reqwest::Response) -> Result<Vec<SseJsonRpcEvent>> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut messages = Vec::new();
    while let Some(chunk) = stream.next().await {
        append_json_rpc_frames(&mut messages, decoder.push(&chunk?)?)?;
    }
    append_json_rpc_frames(&mut messages, decoder.finish()?)?;
    Ok(messages)
}

fn append_json_rpc_frames(
    messages: &mut Vec<SseJsonRpcEvent>,
    frames: Vec<SseFrame>,
) -> Result<()> {
    for frame in frames {
        if let Some(data) = frame.data {
            messages.push(SseJsonRpcEvent {
                message: parse_json_rpc_str(&data)?,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_one_byte_chunks_split_utf8_bom_and_multiline_data() {
        let input = "\u{feff}: comment\r\nid: stream:1\rdata: h\u{e9}ello \u{e9}\rdata: world\r\nretry: 1250\r\n\r\n";
        let mut decoder = SseDecoder::default();
        let mut frames = Vec::new();
        for byte in input.as_bytes() {
            frames.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.as_deref(), Some("héello é\nworld"));
    }

    #[test]
    fn ignores_obsolete_event_ids() {
        let mut decoder = SseDecoder::default();
        let frames = decoder
            .push(b"id: prime:0\n\nid: bad\0id\ndata: {}\n\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.as_deref(), Some("{}"));
    }

    #[test]
    fn rejects_invalid_or_incomplete_utf8() {
        assert!(SseDecoder::default().push(&[0xff]).is_err());
        let mut decoder = SseDecoder::default();
        decoder.push(&[0xc3]).unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn rejects_oversized_lines_and_events() {
        let line = vec![b'x'; DEFAULT_MAX_SSE_LINE_BYTES + 1];
        assert!(SseDecoder::default().push(&line).is_err());
        let mut event = b"data: ".to_vec();
        event.extend(vec![b'x'; DEFAULT_MAX_MESSAGE_BYTES + 1]);
        event.extend_from_slice(b"\n\n");
        assert!(SseDecoder::default().push(&event).is_err());
    }

    #[test]
    fn explicit_limits_accept_larger_single_line_events() {
        let mut event = b"data: ".to_vec();
        event.extend(vec![b'x'; DEFAULT_MAX_SSE_LINE_BYTES + 1]);
        event.extend_from_slice(b"\n\n");
        let mut decoder = SseDecoder::with_limits(event.len(), event.len());
        let frames = decoder.push(&event).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].data.as_ref().map(String::len),
            Some(DEFAULT_MAX_SSE_LINE_BYTES + 1)
        );
    }
}
