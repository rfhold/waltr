# Google speech contracts

`google/` contains the complete, unmodified 17-file schema closure for:

- `google.cloud.speech.v2.Speech` (`google/cloud/speech/v2/cloud_speech.proto`).
- `google.cloud.texttospeech.v1.TextToSpeech` (`google/cloud/texttospeech/v1/cloud_tts.proto`).

Every transitive import exists locally. Generation requires no source downloads or Buf registry dependencies.

## Provenance and licenses

The source files and licenses match the sibling `rfhold/whispers` repository's `proto/` directory byte-for-byte.

Files outside `google/protobuf/` come from [googleapis/googleapis](https://github.com/googleapis/googleapis/tree/e7513c828b8db536fa12fc5493d541f323aedf90).
The exact commit is `e7513c828b8db536fa12fc5493d541f323aedf90`.
[LICENSE.googleapis](LICENSE.googleapis) contains the upstream Apache License 2.0.

Files under `google/protobuf/` come from [protocolbuffers/protobuf v33.0](https://github.com/protocolbuffers/protobuf/tree/a79f2d2e9fadd75e94f3fe40a0399bf0a5d90551/src/google/protobuf).
The exact commit is `a79f2d2e9fadd75e94f3fe40a0399bf0a5d90551`.
[LICENSE.protobuf](LICENSE.protobuf) contains the upstream BSD license.
Each source file retains its upstream copyright and license header.

Do not edit vendor schemas or add local schemas under `google/`.
Buf applies `STANDARD` lint rules outside that directory.

## Generation and Rust API

Run `buf lint` and `buf generate` from the repository root.
Cargo uses committed Prost/Tonic output under `crates/waltr/src/grpc/google/`; no build script generates these contracts.
Tonic retains `no_transport=true`; callers supply their own transport.
Well-known protobuf message types use `prost_types`, with no duplicate generated definitions.
All upstream RPCs remain in the generated clients and server traits.

The Prost option `disable_comments=.google` omits upstream message comments from generated Rust.
Those comments contain protobuf and YAML examples that Rustdoc treats as Rust doctests.
The vendor schemas retain the complete documentation; Waltr doctests remain enabled.

Public packages include `waltr::grpc::google::{api, cloud, longrunning, rpc}`.
`waltr_audio::stt` reexports Speech v2 messages and `SpeechClient`.
`waltr_audio::tts` reexports Text-to-Speech v1 messages and `TextToSpeechClient`.
These imports add no connection setup, authentication, or audio operations.
