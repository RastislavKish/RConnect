#!/bin/sh
# Compiles eSpeak NG's voice data with a native eSpeak NG build and copies it to
# <output dir>/espeak-ng-data.
# Usage: build-espeak-data.sh <output dir>
set -eu
out=$(realpath -m "$1")
root=$(cd "$(dirname "$0")/../.." && pwd)
build="$root/target/espeak-ng-data-build"
mkdir -p "$build"
log="$build/build.log"
if ! {
    cmake -S "$root/mobile/third_party/espeak-ng" -B "$build" -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF -DENABLE_TESTS=OFF -DUSE_SPEECHPLAYER=OFF -DUSE_ASYNC=OFF \
        -DUSE_MBROLA=OFF -DUSE_LIBSONIC=OFF -DUSE_LIBPCAUDIO=OFF -DESPEAK_BUILD_MANPAGES=OFF \
        "-DFETCHCONTENT_SOURCE_DIR_SONIC-GIT=$root/mobile/third_party/sonic" \
        -DFETCHCONTENT_FULLY_DISCONNECTED=ON &&
    cmake --build "$build" --target data --parallel
} > "$log" 2>&1; then
    cat "$log" >&2
    exit 1
fi
rm -rf "$out/espeak-ng-data"
mkdir -p "$out"
cp -r "$build/espeak-ng-data" "$out/"
