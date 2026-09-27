#!/bin/sh
# Builds the Rust core for Android into <output dir>/<abi>/librconnect_ffi.so.
# Usage: build-rust.sh <output dir> [abi...]    (default abis: arm64-v8a x86_64)
set -eu
out=$(realpath -m "$1"); shift
[ $# -gt 0 ] || set -- arm64-v8a x86_64
cd "$(dirname "$0")/../.."
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    sdk=${ANDROID_HOME:-$HOME/Android/Sdk}
    ANDROID_NDK_HOME=$(ls -d "$sdk"/ndk/* | sort -V | tail -1)
    export ANDROID_NDK_HOME
fi
targets=""
for abi in "$@"; do targets="$targets -t $abi"; done
# shellcheck disable=SC2086
cargo ndk $targets -P 28 -o "$out" build --release -p rconnect-ffi --lib
