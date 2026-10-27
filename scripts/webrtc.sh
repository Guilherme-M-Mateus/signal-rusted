#!/bin/sh
# Prepares Signal's prebuilt WebRTC for `cargo build --features calls` (see docs/calls.md):
# downloads it with RingRTC's own bin/fetch-artifact (which checks its sha256), then renames
# the BoringSSL symbols that clash with the system OpenSSL. Without the rename, SQLCipher's
# OpenSSL calls bind to WebRTC's BoringSSL copy and opening the database crashes.
set -eu
cd "$(dirname "$0")/.."
out="$PWD/target/webrtc"

# RingRTC as cargo checked it out (Cargo.toml pins the tag).
manifest=$(cargo metadata --format-version 1 --features calls --filter-platform "$(rustc -vV | sed -n "s/host: //p")" |
    python3 -c 'import json,sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "ringrtc"))')
ringrtc="$(dirname "$manifest")/../.."

mkdir -p "$out/download"
OUTPUT_DIR="$out/download" "$ringrtc/bin/fetch-artifact" --platform desktop
lib="$out/download/release/obj/libwebrtc.a"

ssl_dir=$(pkg-config --variable=libdir libcrypto)
nm -D --defined-only "$ssl_dir/libcrypto.so" "$ssl_dir/libssl.so" | awk 'NF == 3 { sub(/@.*/, "", $3); print $3 }' | sort -u >"$out/openssl.syms"
llvm-nm -g --defined-only "$lib" 2>/dev/null | awk 'NF == 3 { print $3 }' | sort -u >"$out/webrtc.syms"
comm -12 "$out/openssl.syms" "$out/webrtc.syms" | awk '{ print $1 " signalrs_webrtc_" $1 }' >"$out/rename.map"

# RingRTC's build script picks the directory by cargo profile; the prebuilt is release-only.
mkdir -p "$out/release/obj" "$out/debug/obj"
# llvm-objcopy: GNU objcopy can't read the archive's CREL relocations.
llvm-objcopy --redefine-syms="$out/rename.map" "$lib" "$out/release/obj/libwebrtc.a"
cp "$out/release/obj/libwebrtc.a" "$out/debug/obj/libwebrtc.a"
echo "libwebrtc.a ready in $out ($(wc -l <"$out/rename.map") symbols renamed)"
