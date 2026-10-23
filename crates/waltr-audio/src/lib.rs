//! Bounded, cancellable adapters for Google Speech v2 and Text-to-Speech v1.
//! No authentication, device I/O, resampling, retries, or provider discovery.
//! Generated messages and clients remain available in [`stt`] and [`tts`].
//!
//! ```no_run
//! use futures_util::{stream, StreamExt};
//! use waltr_audio::{CancellationToken, audio::PcmS16Le, client::Client,
//!     config::Config, stt, tts};
//!
//! # async fn example() -> Result<(), waltr_audio::error::Error> {
//! let client = Client::connect(Config::new("https://speech.example.com")).await?;
//! let cancel = CancellationToken::new();
//! let format = PcmS16Le::new(16_000, 1)?;
//! let result = client.recognize("projects/p/locations/l/recognizers/r".into(),
//!     stt::RecognitionConfig::default(), format, vec![0, 0], cancel.clone()).await?;
//! let mut results = client.streaming_recognize("projects/p/locations/l/recognizers/r".into(),
//!     stt::StreamingRecognitionConfig::default(), format,
//!     stream::iter([Ok(vec![0, 0])]), cancel.clone())?;
//! while let Some(response) = results.next().await {
//!     let response = response?; // Complete upstream result, not flattened text.
//! }
//! let audio = client.synthesize_speech(tts::SynthesizeSpeechRequest {
//!     input: Some(tts::SynthesisInput {
//!         input_source: Some(tts::synthesis_input::InputSource::Text("hello".into())),
//!         ..Default::default()
//!     }),
//!     audio_config: Some(tts::AudioConfig {
//!         audio_encoding: tts::AudioEncoding::Linear16 as i32,
//!         ..Default::default()
//!     }),
//!     ..Default::default() // Set upstream voice/model/language fields as required.
//! }, cancel.clone()).await?; // `audio.container` is WAV, not raw PCM.
//! let mut audio_chunks = client.streaming_synthesize(tts::StreamingSynthesizeConfig {
//!     streaming_audio_config: Some(tts::StreamingAudioConfig {
//!         audio_encoding: tts::AudioEncoding::Pcm as i32,
//!         ..Default::default()
//!     }),
//!     ..Default::default()
//! }, stream::iter([Ok(tts::StreamingSynthesisInput {
//!     input_source: Some(tts::streaming_synthesis_input::InputSource::Text("hello".into())),
//!     ..Default::default()
//! })]), cancel.clone())?;
//! cancel.cancel(); // Stops producer and upstream RPC; response drop also stops both.
//! # Ok(())
//! # }
//! ```
#![allow(clippy::missing_errors_doc)]

pub mod audio;
pub mod client;
pub mod config;
pub mod error;
pub mod stt;
mod telemetry;
pub mod tts;

pub use tokio_util::sync::CancellationToken;
