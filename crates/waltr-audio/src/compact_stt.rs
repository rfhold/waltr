//! Concrete Speech v2 wire-backed responses. No generated word vectors are decoded.
//!
//! A caller must reserve `MAX_FRAME_BYTES` plus its formatting workspace before
//! starting this RPC. Transport buffering and allocator metadata are not that budget.
use crate::{
    audio::PcmS16Le,
    client::{Client, ResponseGate, Responses, duplex_gated},
    error::Error,
    stt,
    telemetry::Rpc,
};
use futures_core::Stream;
use futures_util::{StreamExt, stream};
use prost::{Message, bytes::Buf};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// Maximum owned wire frame, admitted before its copy.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Maximum aggregate response wire bytes in one operation (including discarded fields).
pub const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
/// Finite response count, including empty protobuf frames.
pub const MAX_FRAMES: usize = 65536;
const MAX_FIELDS: usize = 2 * 1024 * 1024;
const MAX_WORDS: usize = 524_288;
const MAX_TEXT: usize = 1024 * 1024;

/// One validated response. Holding it, even after checkout, prevents the next decode.
/// It cannot be cloned; borrowed views cannot outlive its credit.
pub struct Response {
    wire: Box<[u8]>,
    permit: Option<OwnedSemaphorePermit>,
}
impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("wire_bytes", &self.wire.len())
            .finish_non_exhaustive()
    }
}
impl Response {
    /// Actual retained wire payload bytes; allocator metadata is excluded.
    #[must_use]
    pub fn wire_bytes(&self) -> usize {
        self.wire.len()
    }
    /// Validated streaming results, without per-result or per-word allocation.
    pub fn results(&self) -> impl Iterator<Item = ResultView<'_>> {
        messages(&self.wire, 6).map(ResultView)
    }
}
/// Borrowed streaming result. Unknown fields remain in its bounded parent frame.
#[derive(Clone, Copy, Debug)]
pub struct ResultView<'a>(&'a [u8]);
impl<'a> ResultView<'a> {
    /// Portion-final marker; not an authoritative diarization commit.
    #[must_use]
    pub fn is_final(self) -> bool {
        number(self.0, 2) == 1
    }
    /// Recognition channel, not a speaker ID.
    ///
    /// # Panics
    /// Only if the private validated-wire invariant is violated internally.
    #[must_use]
    pub fn channel(self) -> u32 {
        u32::try_from(number(self.0, 5)).expect("validated channel")
    }
    /// Alternatives in provider order.
    pub fn alternatives(self) -> impl Iterator<Item = Alternative<'a>> {
        messages(self.0, 1).map(Alternative)
    }
}
/// Borrowed recognition alternative.
#[derive(Clone, Copy, Debug)]
pub struct Alternative<'a>(&'a [u8]);
impl<'a> Alternative<'a> {
    /// Portion transcript, independently validated from cumulative words.
    #[must_use]
    pub fn transcript(self) -> &'a str {
        text(self.0, 1)
    }
    /// Cumulative words with zero per-word allocations.
    pub fn words(self) -> impl Iterator<Item = Word<'a>> {
        messages(self.0, 3).map(Word)
    }
}
/// Borrowed word and request-local speaker attribution.
#[derive(Clone, Copy, Debug)]
pub struct Word<'a>(&'a [u8]);
impl<'a> Word<'a> {
    /// Nonempty single whitespace-delimited word.
    #[must_use]
    pub fn word(self) -> &'a str {
        text(self.0, 3)
    }
    /// Bounded upstream speaker label, possibly empty.
    #[must_use]
    pub fn speaker(self) -> &'a str {
        text(self.0, 6)
    }
}

#[derive(Clone, Copy)]
enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
    Fixed64,
    Group(usize),
}
struct Fields<'a>(&'a [u8]);
fn varint(bytes: &mut &[u8]) -> Result<u64, tonic::Status> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or_else(invalid)?;
        *bytes = rest;
        if shift == 63 && byte > 1 {
            return Err(invalid());
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(invalid())
}
fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8], tonic::Status> {
    let part = bytes.get(..length).ok_or_else(invalid)?;
    *bytes = &bytes[length..];
    Ok(part)
}
fn skip_group(bytes: &mut &[u8], opening: u32, depth: usize) -> Result<usize, tonic::Status> {
    if depth > 4 {
        return Err(invalid());
    }
    let mut fields = 0;
    loop {
        let key = varint(bytes)?;
        let tag = u32::try_from(key >> 3).map_err(|_| invalid())?;
        if tag == 0 || tag > 0x1fff_ffff {
            return Err(invalid());
        }
        fields += 1;
        if fields > MAX_FIELDS {
            return Err(invalid());
        }
        match key & 7 {
            0 => {
                varint(bytes)?;
            }
            1 => {
                take(bytes, 8)?;
            }
            2 => {
                let len = usize::try_from(varint(bytes)?).map_err(|_| invalid())?;
                take(bytes, len)?;
            }
            3 => {
                fields += skip_group(bytes, tag, depth + 1)?;
            }
            4 => {
                return if tag == opening {
                    Ok(fields)
                } else {
                    Err(invalid())
                };
            }
            5 => {
                take(bytes, 4)?;
            }
            _ => return Err(invalid()),
        }
    }
}
impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), tonic::Status>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        Some((|| {
            let key = varint(&mut self.0)?;
            let tag = u32::try_from(key >> 3).map_err(|_| invalid())?;
            if tag == 0 || tag > 0x1fff_ffff {
                return Err(invalid());
            }
            let value = match key & 7 {
                0 => Value::Number(varint(&mut self.0)?),
                1 => {
                    take(&mut self.0, 8)?;
                    Value::Fixed64
                }
                2 => {
                    let len = usize::try_from(varint(&mut self.0)?).map_err(|_| invalid())?;
                    Value::Bytes(take(&mut self.0, len)?)
                }
                3 => Value::Group(skip_group(&mut self.0, tag, 1)?),
                5 => Value::Fixed32(u32::from_le_bytes(
                    take(&mut self.0, 4)?.try_into().map_err(|_| invalid())?,
                )),
                _ => return Err(invalid()),
            };
            Ok((tag, value))
        })())
    }
}
fn invalid() -> tonic::Status {
    tonic::Status::invalid_argument("invalid or over-budget compact speech response")
}
fn messages(bytes: &[u8], field: u32) -> impl Iterator<Item = &[u8]> {
    Fields(bytes).filter_map(move |item| match item.expect("validated wire") {
        (tag, Value::Bytes(v)) if tag == field => Some(v),
        _ => None,
    })
}
fn text(bytes: &[u8], field: u32) -> &str {
    messages(bytes, field)
        .next()
        .map_or("", |v| std::str::from_utf8(v).expect("validated UTF-8"))
}
fn number(bytes: &[u8], field: u32) -> u64 {
    Fields(bytes)
        .find_map(|item| match item.expect("validated wire") {
            (tag, Value::Number(v)) if tag == field => Some(v),
            _ => None,
        })
        .unwrap_or(0)
}
#[derive(Clone, Copy)]
enum Schema {
    Response,
    Result,
    Alternative,
    Word,
    Duration,
    Metadata,
}
struct Scan {
    fields: usize,
    words: usize,
    results: usize,
}
impl Schema {
    const fn child(self, tag: u32) -> Option<(Self, bool)> {
        match (self, tag) {
            (Self::Response, 6) => Some((Self::Result, true)),
            (Self::Response, 7) | (Self::Result, 4) | (Self::Word, 1 | 2) | (Self::Metadata, 6) => {
                Some((Self::Duration, false))
            }
            (Self::Response, 5) => Some((Self::Metadata, false)),
            (Self::Result, 1) => Some((Self::Alternative, true)),
            (Self::Alternative, 3) => Some((Self::Word, true)),
            _ => None,
        }
    }
    const fn string_max(self, tag: u32) -> Option<usize> {
        match (self, tag) {
            (Self::Alternative, 1) | (Self::Metadata, 10) => Some(MAX_TEXT),
            (Self::Word, 3) | (Self::Metadata, 9) => Some(1024),
            (Self::Word | Self::Result, 6) => Some(64),
            _ => None,
        }
    }
    const fn number_max(self, tag: u32) -> Option<u64> {
        match (self, tag) {
            (Self::Response, 3) => Some(i32::MAX as u64),
            (Self::Result, 2) => Some(1),
            (Self::Result, 5) => Some(8),
            (Self::Duration, 1) => Some(5400),
            (Self::Duration, 2) => Some(999_999_999),
            _ => None,
        }
    }
}

fn validate(bytes: &[u8], schema: Schema, scan: &mut Scan) -> Result<(), tonic::Status> {
    let mut seen = 0u64;
    let mut repeated = 0;
    for field in Fields(bytes) {
        scan.fields += 1;
        if scan.fields > MAX_FIELDS {
            return Err(invalid());
        }
        let (tag, value) = field?;
        if let Value::Group(fields) = value {
            scan.fields += fields;
            if scan.fields > MAX_FIELDS {
                return Err(invalid());
            }
        }
        let child = schema.child(tag);
        let string_max = schema.string_max(tag);
        let max_number = schema.number_max(tag);
        let float = matches!(
            (schema, tag),
            (Schema::Alternative, 2) | (Schema::Word, 4) | (Schema::Result, 3)
        );
        let known = child.is_some() || string_max.is_some() || max_number.is_some() || float;
        if known && !child.is_some_and(|(_, repeated)| repeated) {
            let bit = 1u64 << tag;
            if seen & bit != 0 {
                return Err(invalid());
            }
            seen |= bit;
        }
        if let Some((child, is_repeated)) = child {
            let Value::Bytes(bytes) = value else {
                return Err(invalid());
            };
            if is_repeated {
                repeated += 1;
                let maximum = scan.repeated_max(schema)?;
                if repeated > maximum {
                    return Err(invalid());
                }
            }
            validate(bytes, child, scan)?;
        } else if let Some(maximum) = string_max {
            let Value::Bytes(bytes) = value else {
                return Err(invalid());
            };
            if bytes.len() > maximum {
                return Err(invalid());
            }
            let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
            if matches!((schema, tag), (Schema::Word, 3))
                && (text.is_empty() || text.split_whitespace().count() != 1 || text.trim() != text)
            {
                return Err(invalid());
            }
        } else if let Some(maximum) = max_number {
            let Value::Number(n) = value else {
                return Err(invalid());
            };
            if n > maximum {
                return Err(invalid());
            }
        } else if float {
            let Value::Fixed32(bits) = value else {
                return Err(invalid());
            };
            let f = f32::from_bits(bits);
            if !f.is_finite() || !(0.0..=1.0).contains(&f) {
                return Err(invalid());
            }
        }
    }
    validate_complete(bytes, schema, seen)
}

impl Scan {
    fn repeated_max(&mut self, schema: Schema) -> Result<usize, tonic::Status> {
        match schema {
            Schema::Response => {
                self.results += 1;
                Ok(4096)
            }
            Schema::Result => Ok(30),
            Schema::Alternative => {
                self.words += 1;
                if self.words > MAX_WORDS {
                    return Err(invalid());
                }
                Ok(MAX_WORDS)
            }
            _ => Err(invalid()),
        }
    }
}

fn validate_complete(bytes: &[u8], schema: Schema, seen: u64) -> Result<(), tonic::Status> {
    // Omitted proto3 word strings default to empty, which is not a word.
    if matches!(schema, Schema::Word) && seen & (1 << 3) == 0 {
        return Err(invalid());
    }
    if matches!(schema, Schema::Duration) && number(bytes, 1) == 5400 && number(bytes, 2) != 0 {
        return Err(invalid());
    }
    if matches!(schema, Schema::Word) {
        let start = messages(bytes, 1).next();
        let end = messages(bytes, 2).next();
        if let (Some(start), Some(end)) = (start, end) {
            let nanos = |duration: &[u8]| number(duration, 1) * 1_000_000_000 + number(duration, 2);
            if nanos(start) > nanos(end) {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

struct SpeechCodec {
    total: usize,
    frames: usize,
    frame_limit: usize,
}
impl SpeechCodec {
    fn admit(&mut self, len: usize) -> Result<(), tonic::Status> {
        if len > self.frame_limit {
            return Err(invalid());
        }
        self.total = self
            .total
            .checked_add(len + 5)
            .filter(|n| *n <= MAX_TOTAL_BYTES)
            .ok_or_else(invalid)?;
        self.frames = self
            .frames
            .checked_add(1)
            .filter(|n| *n <= MAX_FRAMES)
            .ok_or_else(invalid)?;
        Ok(())
    }
}
struct RequestEncoder;
impl Encoder for RequestEncoder {
    type Item = stt::StreamingRecognizeRequest;
    type Error = tonic::Status;
    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(dst).map_err(|_| invalid())
    }
}
impl Decoder for SpeechCodec {
    type Item = Response;
    type Error = tonic::Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Response>, Self::Error> {
        let len = src.remaining();
        if src.chunk().len() != len {
            return Err(invalid());
        }
        self.admit(len)?;
        validate(
            src.chunk(),
            Schema::Response,
            &mut Scan {
                fields: 0,
                words: 0,
                results: 0,
            },
        )?;
        // Exact-sized allocation only after validation and while the driver holds
        // the exclusive frame gate. No shared transport slab is retained.
        let wire = src.chunk().into();
        src.advance(len);
        Ok(Some(Response { wire, permit: None }))
    }
}
impl Codec for SpeechCodec {
    type Encode = stt::StreamingRecognizeRequest;
    type Decode = Response;
    type Encoder = RequestEncoder;
    type Decoder = Self;
    fn encoder(&mut self) -> RequestEncoder {
        RequestEncoder
    }
    fn decoder(&mut self) -> Self {
        Self {
            total: 0,
            frames: 0,
            frame_limit: self.frame_limit,
        }
    }
}

impl Client {
    /// Opt-in bounded wire-backed Speech v2 RPC. The caller must pre-reserve an
    /// 8 MiB frame allowance. Only one response may be retained; drop it before
    /// requesting another. Known fields reject duplicates, invalid UTF-8, negative
    /// offsets/channels, excessive repeated counts, and invalid numeric ranges.
    /// Unknown fields are skipped within the same byte/field budgets; legacy groups
    /// must have matched tags and at most four nested group levels.
    #[allow(clippy::too_many_arguments)]
    pub fn streaming_recognize_compact<S>(
        &self,
        recognizer: String,
        mut config: stt::StreamingRecognitionConfig,
        format: PcmS16Le,
        input: S,
        cancel: CancellationToken,
    ) -> Result<Responses<Response>, Error>
    where
        S: Stream<Item = Result<Vec<u8>, Error>> + Send + 'static,
    {
        stt::decoding(config.config.get_or_insert_default(), format);
        if let Some(mask) = &mut config.config_mask
            && !mask
                .paths
                .iter()
                .any(|p| p == "*" || p == "explicit_decoding_config")
        {
            mask.paths.push("explicit_decoding_config".into());
        }
        let first = stt::StreamingRecognizeRequest {
            recognizer,
            streaming_request: Some(
                stt::streaming_recognize_request::StreamingRequest::StreamingConfig(config),
            ),
        };
        let max_bytes = self.config.queue_bytes.min(self.config.max_encoding_bytes);
        let input = pcm_fragments(input, format, max_bytes)?;
        let frame_limit = self.config.max_decoding_bytes.min(MAX_FRAME_BYTES);
        let mut grpc = tonic::client::Grpc::new(self.channel.clone())
            .max_encoding_message_size(self.config.max_encoding_bytes)
            .max_decoding_message_size(frame_limit);
        duplex_gated(
            &self.config,
            Rpc::StreamingRecognize,
            cancel,
            first,
            input,
            |bytes| Ok(stt::audio_request(bytes)),
            move |request| async move {
                grpc.ready()
                    .await
                    .map_err(|_| tonic::Status::unavailable("speech transport not ready"))?;
                let mut request = request;
                request.extensions_mut().insert(tonic::GrpcMethod::new(
                    "google.cloud.speech.v2.Speech",
                    "StreamingRecognize",
                ));
                grpc.streaming(
                    request,
                    tonic::codegen::http::uri::PathAndQuery::from_static(
                        "/google.cloud.speech.v2.Speech/StreamingRecognize",
                    ),
                    SpeechCodec {
                        total: 0,
                        frames: 0,
                        frame_limit,
                    },
                )
                .await
            },
            Some(ResponseGate {
                semaphore: Arc::new(Semaphore::new(1)),
                attach: |mut response, permit| {
                    response.permit = Some(permit);
                    response
                },
            }),
        )
    }
}

fn pcm_fragments<S>(
    input: S,
    format: PcmS16Le,
    max_bytes: usize,
) -> Result<impl Stream<Item = Result<Vec<u8>, Error>> + Send, Error>
where
    S: Stream<Item = Result<Vec<u8>, Error>> + Send + 'static,
{
    let frame = usize::try_from(format.channels())
        .ok()
        .and_then(|n| n.checked_mul(2))
        .filter(|n| *n <= 15360)
        .ok_or(Error::Invalid("invalid PCM frame"))?;
    let fragment = 15360 / frame * frame;
    Ok(stream::try_unfold(
        (Box::pin(input), None::<(Vec<u8>, usize)>),
        move |(mut source, pending)| async move {
            let (bytes, offset) = if let Some(pending) = pending {
                pending
            } else {
                let Some(bytes) = source.next().await else {
                    return Ok(None);
                };
                let bytes = bytes?;
                format.validate(&bytes)?;
                let request = stt::audio_request(bytes);
                if request.encoded_len() > max_bytes {
                    return Err(Error::Invalid(
                        "input chunk exceeds queue or encoding limit",
                    ));
                }
                let Some(stt::streaming_recognize_request::StreamingRequest::Audio(bytes)) =
                    request.streaming_request
                else {
                    unreachable!()
                };
                (bytes, 0)
            };
            if offset == 0 && bytes.len() <= fragment {
                return Ok(Some((bytes, (source, None))));
            }
            let end = offset.saturating_add(fragment).min(bytes.len());
            let part = bytes[offset..end].to_vec();
            Ok(Some((
                part,
                (source, (end < bytes.len()).then_some((bytes, end))),
            )))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_before_owned_copy() {
        for bytes in [
            &[0x32, 2, 0x28, 0xff][..],
            &[0x32, 2, 0x10, 2],
            &[0x32, 4, 0x10, 1, 0x10, 1],
            &[0x32, 5, 0x0a, 3, 0x0a, 1, 0xff],
        ] {
            assert!(
                validate(
                    bytes,
                    Schema::Response,
                    &mut Scan {
                        fields: 0,
                        words: 0,
                        results: 0
                    }
                )
                .is_err()
            );
        }
    }
    #[test]
    fn unknown_fields_groups_and_duration_order_are_bounded() {
        let valid = |bytes: &[u8]| {
            validate(
                bytes,
                Schema::Response,
                &mut Scan {
                    fields: 0,
                    words: 0,
                    results: 0,
                },
            )
        };
        assert!(valid(&[0xa2, 6, 2, 255, 255]).is_ok()); // Unknown opaque bytes, not a UTF-8 field.
        assert!(valid(&[0x53, 8, 1, 0x54]).is_ok()); // Matched unknown legacy group.
        assert!(valid(&[0x53, 0x5c]).is_err());
        assert!(valid(&[0x53, 0x53, 0x53, 0x53, 0x53, 0x54, 0x54, 0x54, 0x54, 0x54]).is_err());
        let word = &[10, 2, 8, 2, 18, 2, 8, 1, 26, 1, b'a'];
        assert!(
            validate(
                word,
                Schema::Word,
                &mut Scan {
                    fields: 0,
                    words: 0,
                    results: 0
                }
            )
            .is_err()
        );
        assert!(
            validate(
                &[8, 0x98, 0x2a, 16, 1],
                Schema::Duration,
                &mut Scan {
                    fields: 0,
                    words: 0,
                    results: 0
                }
            )
            .is_err()
        );
        assert!(valid(&[0x32, 4, 0x10, 1, 0x10, 1]).is_err());
        let mut too_many = Vec::new();
        for _ in 0..4097 {
            too_many.extend_from_slice(&[0x32, 0]);
        }
        assert!(valid(&too_many).is_err());
    }
    #[test]
    fn exact_field_and_alternative_limits_accept_then_reject_one_more() {
        let valid = |bytes: &[u8], schema| {
            validate(
                bytes,
                schema,
                &mut Scan {
                    fields: 0,
                    words: 0,
                    results: 0,
                },
            )
        };
        let mut fields = [80, 0].repeat(MAX_FIELDS);
        assert!(fields.len() < MAX_FRAME_BYTES);
        assert!(valid(&fields, Schema::Response).is_ok());
        fields.extend_from_slice(&[80, 0]);
        assert!(valid(&fields, Schema::Response).is_err());
        let mut alternatives = [10, 0].repeat(30);
        assert!(valid(&alternatives, Schema::Result).is_ok());
        alternatives.extend_from_slice(&[10, 0]);
        assert!(valid(&alternatives, Schema::Result).is_err());
        let mut results = [0x32, 0].repeat(4096);
        assert!(valid(&results, Schema::Response).is_ok());
        results.extend_from_slice(&[0x32, 0]);
        assert!(valid(&results, Schema::Response).is_err());
    }
    #[test]
    fn frame_and_total_wire_quotas_include_empty_frames() {
        let mut codec = SpeechCodec {
            total: 0,
            frames: 0,
            frame_limit: MAX_FRAME_BYTES,
        };
        assert!(codec.admit(MAX_FRAME_BYTES + 1).is_err());
        codec.total = MAX_TOTAL_BYTES - 5;
        codec.admit(0).unwrap();
        assert!(codec.admit(0).is_err());
        codec.total = 0;
        codec.frames = MAX_FRAMES - 1;
        codec.admit(0).unwrap();
        assert!(codec.admit(0).is_err());
    }
}
