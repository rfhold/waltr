# Interpreter artifact provenance

## Inputs

| Input | Revision or digest |
| --- | --- |
| QuickJS-NG | 0.16.2 |
| Source | <https://github.com/quickjs-ng/quickjs/releases/download/v0.16.2/quickjs-amalgam.zip> |
| Source archive SHA-256 | `866bad1e2f148b6f8144a4a3d4c3f8a0754495a6920fe56a7dd698cf0ad27f65` |
| Compiler | WASI SDK 27.0, Apple Silicon macOS host |
| SDK archive SHA-256 | `055c3dc2766772c38e71a05d353e35c322c7b2c6458a36a26a836f9808a550f8` |
| Clang | `20.1.8-wasi-sdk`, LLVM `87f0227cb60147a26a1eeb4fb06e3b505e9c7261` |
| Compiler target | `wasm32-unknown-wasi` |
| C bridge SHA-256 | `6b41e37035cf9fb473d8cb9fde2ab2456c6f875468e48e3f58dde106245566aa` |
| Budget hook header SHA-256 | `879332bcaeecfd89c781433da2d8a917a2fe7c6ee4684ce102965f60609ad217` |
| QuickJS patch SHA-256 | `92dc3b885f6fb6f8312cd2cf69c95285bc2ed2228b2541297e0a01234859112d` |
| Rebuild script SHA-256 | `b3dcc05ba57be99fafd00c6a87178c0a0a4d9e73318b58ca6e38300354294a34` |

The source archive contains `quickjs-amalgam.c`, `quickjs.h`, and `quickjs-libc.h`.
The build uses the engine amalgam, not the QuickJS command-line executable or libc integration layer.
The bridge source and compiler arguments belong to this crate.
The tracked [QuickJS patch](wasm/quickjs-budget.patch) enables the upstream-disabled WASI C-stack limit and records native budget events through [budgets.h](wasm/budgets.h).
The hook covers heap-limit checks, C-stack checks, and guest out-of-memory reporting; it reads no JavaScript error text.
The prior disposable experiment artifact is not a production input.

## Output

| Artifact | Value |
| --- | --- |
| Path | [artifacts/quickjs.wasm](artifacts/quickjs.wasm) |
| Bytes | 1,331,759 |
| SHA-256 | `8af33f7c1f38198cc3bec5b82ae02d2f6e5f1aa174f8fcb34fb716767f1e4e7d` |
| Independent checksum manifest | [artifacts/SHA256SUMS](artifacts/SHA256SUMS) |
| WASM maximum linear memory | 32 MiB |
| WASM C stack reservation | 1 MiB |

Cargo embeds this artifact through `include_bytes!`.
End-user builds and execution require no C compiler, source archive, WASI SDK, download, or native QuickJS library.

## Rebuild

Use absolute paths for both external inputs:

```sh
QUICKJS_ARCHIVE=/absolute/path/quickjs-amalgam.zip \
WASI_SDK_PATH=/absolute/path/wasi-sdk-27.0-arm64-macos \
sh crates/waltr-script/scripts/rebuild.sh
```

The script verifies the source archive digest before compilation.
It requires the SDK 27.0 version record and its pinned LLVM revision.
It applies the tracked patch to the freshly extracted amalgam with zero fuzz before compilation.
It checks the bridge with `-Wall -Wextra -Werror -fsyntax-only`.
It compiles both C sources with `-O2`, `_GNU_SOURCE`, and `_WASI_EMULATED_SIGNAL`.
It links a WASI reactor with the fixed stack and maximum memory limits.
It links `wasi-emulated-signal` and `m`.
It writes only crate-local build caches, the artifact, and its checksum manifest.

## Import boundary

The Rust loader allows only these `wasi_snapshot_preview1` imports:

- `clock_time_get`: zero clock bytes.
- `fd_close`: error 76.
- `fd_fdstat_get`: error 76.
- `fd_seek`: error 76.
- `fd_write`: error 76.

No WASI configuration, preopened directory, environment, argument, socket, random device, or host file descriptor enters the guest.
The C reactor exposes private protocol functions only to Rust.
JavaScript accesses neither those exports nor the WASM memory.

## Licenses and platform evidence

QuickJS-NG engine code uses the MIT license; its amalgam also includes Unicode data under Unicode License V3.
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) preserves the MIT notices and the complete Unicode copyright and permission notice from the pinned amalgam.
WASI SDK supplies the libc, signal emulation, and compiler runtime portions.
Their upstream license references are [wasi-libc](https://github.com/WebAssembly/wasi-libc/blob/main/LICENSE) and [LLVM](https://github.com/llvm/llvm-project/blob/llvmorg-20.1.8/LICENSE.TXT).
The WASI libc, signal-emulation, and compiler-runtime distribution license audit remains open; these references do not complete it.

The local macOS SDK build and Wasmi execution provide the current platform evidence.
No Linux, iOS, simulator, device, or application package claim follows from that evidence.
