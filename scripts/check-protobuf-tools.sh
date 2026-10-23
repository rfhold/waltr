#!/bin/sh
set -eu

PROST_PLUGIN_VERSION=0.5.0
TONIC_PLUGIN_VERSION=0.5.0

case "${1:---check}" in
    --versions)
        printf 'protoc-gen-prost %s\nprotoc-gen-tonic %s\n' "$PROST_PLUGIN_VERSION" "$TONIC_PLUGIN_VERSION"
        exit 0
        ;;
    --check) ;;
    *)
        printf 'Usage: sh scripts/check-protobuf-tools.sh [--check|--versions]\n' >&2
        exit 2
        ;;
esac

check_version() {
    tool=$1
    expected=$2
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'Missing %s; install with: cargo install --locked %s --version %s\n' "$tool" "$tool" "$expected" >&2
        return 1
    fi
    actual=$("$tool" --version) || return 1
    if [ "$actual" != "$expected" ]; then
        printf '%s: expected %s, got %s\n' "$tool" "$expected" "$actual" >&2
        return 1
    fi
    printf '%s %s\n' "$tool" "$actual"
}

check_version protoc-gen-prost "$PROST_PLUGIN_VERSION"
check_version protoc-gen-tonic "$TONIC_PLUGIN_VERSION"
