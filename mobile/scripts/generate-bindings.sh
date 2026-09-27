#!/bin/sh
# Generates the Kotlin bindings of the Rust core into <output dir>.
# Usage: generate-bindings.sh <output dir>
set -eu
out=$(realpath -m "$1")
cd "$(dirname "$0")/../.."
# The metadata the generator reads is stripped from release builds, so a debug build of
# the same code is used.
cargo build -p rconnect-ffi --lib
cargo run -p rconnect-ffi --features cli --bin uniffi-bindgen -- \
    generate --library target/debug/librconnect_ffi.so --language kotlin --no-format \
    --out-dir "$out"
