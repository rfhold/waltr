#!/bin/sh
set -eu
: "${QUICKJS_ARCHIVE:?set QUICKJS_ARCHIVE to the QuickJS-NG 0.16.2 amalgam zip}"
: "${WASI_SDK_PATH:?set WASI_SDK_PATH to WASI SDK 27.0}"
cd "$(dirname "$0")/.."
test "$(sed -n '1p' "$WASI_SDK_PATH/VERSION")" = '27.0'
grep -qx 'llvm: 87f0227cb601' "$WASI_SDK_PATH/VERSION"
grep -qx 'llvm-version: 20.1.8' "$WASI_SDK_PATH/VERSION"
printf '%s  %s\n' 866bad1e2f148b6f8144a4a3d4c3f8a0754495a6920fe56a7dd698cf0ad27f65 "$QUICKJS_ARCHIVE" | shasum -a 256 -c -
mkdir -p build/quickjs artifacts
unzip -oq "$QUICKJS_ARCHIVE" -d build/quickjs
patch -p0 --fuzz=0 < wasm/quickjs-budget.patch
"$WASI_SDK_PATH/bin/clang" -Wall -Wextra -Werror -fsyntax-only -Ibuild/quickjs -Iwasm wasm/reactor.c
"$WASI_SDK_PATH/bin/clang" -O2 -D_GNU_SOURCE -D_WASI_EMULATED_SIGNAL \
  -Ibuild/quickjs -Iwasm -include wasm/budgets.h wasm/reactor.c build/quickjs/quickjs-amalgam.c \
  -mexec-model=reactor -Wl,-z,stack-size=1048576 -Wl,--max-memory=33554432 \
  -lwasi-emulated-signal -lm -o artifacts/quickjs.wasm
shasum -a 256 artifacts/quickjs.wasm > artifacts/SHA256SUMS
