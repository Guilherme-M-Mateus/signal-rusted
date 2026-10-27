# Voice calls (feature `calls`)

1:1 audio calls through Signal's RingRTC (v2.73.0) and its prebuilt WebRTC (`8037a`). The
default build doesn't need any of it and shows no call controls.

## Building

Needs `llvm-nm`/`llvm-objcopy` (LLVM), `pkg-config`, OpenSSL dev files, `python3`, `curl`, plus
what RingRTC's audio needs: `cmake`, libpulse, alsa-lib, jack, speexdsp.

```sh
scripts/webrtc.sh                 # once, and again after `cargo clean`
cargo build --features calls      # or: cargo run --features calls
cargo test --features calls
# Optimized: plain `--release` fails to link (LTO vs cubeb's bundled std), so use this profile.
cargo build --profile release-calls --features calls   # → target/release-calls/signal-rs
```

`scripts/webrtc.sh` runs RingRTC's own `bin/fetch-artifact` from cargo's checkout of the pinned
tag (it downloads `build-artifacts.signal.org/libraries/webrtc-8037a-linux-x64-release.tar.bz2`
and checks the sha256 in RingRTC's `config/webrtc_artifact_checksums.json`), then writes a
patched `libwebrtc.a` to `target/webrtc/{debug,release}/obj/`. `.cargo/config.toml` points
RingRTC's build script there (`OUTPUT_DIR`). WebRTC is never compiled from source.

### Why the patched archive

`libwebrtc.a` bundles BoringSSL with unprefixed OpenSSL names (`EVP_CipherInit_ex`,
`PKCS5_PBKDF2_HMAC`, `RAND_bytes`, … 1947 of them). Linked as-is next to SQLCipher (which uses
the system `libcrypto.so.3`), the linker resolves SQLCipher's calls to the static BoringSSL copy
while OpenSSL-3-only calls (`EVP_MAC_*`) still go to libcrypto: opening an encrypted database
segfaulted in a probe binary. The script renames every `libwebrtc.a` symbol that libcrypto/libssl
also export to `signalrs_webrtc_*` (definitions and references alike), after which SQLCipher binds
to libcrypto again (`nm` shows them undefined) and the same probe, and the `dbkey` SQLCipher
tests in `cargo test --features calls`, pass. RingRTC's `prebuilt_webrtc` feature isn't used
because its build script re-extracts the unpatched archive.

Remaining overlaps, same as Signal's own `libringrtc` on Linux: libc++abi's `__cxa_*` (vs
libstdc++) and Opus (vs a transitively loaded `libopus.so`, unused by us).

## How it works

- `src/calls.rs`: what the UI sees (`Call`, `Phase`, `CallCommand`), always built.
- `src/calls/media.rs`: RingRTC's `CallManager<NativePlatform>`, created on the first call
  (it opens the default microphone/speaker with RingRTC's cubeb audio module); `CallMessage` ⇄
  RingRTC signaling translation (mirrors Desktop's `callingMessageToProto`/`handleCallingMessage`);
  TURN servers from `calling_relays()` mapped like Desktop (IP URLs + hostname first); RingRTC
  states → `Call`.
- `src/backend/calling.rs`: received `CallMessage`s (not ours, for this device) go to RingRTC
  with both identity keys (type byte stripped, as Desktop) and the message age; offers from
  message requests get a `NeedPermission` hangup instead of ringing; RingRTC's signaling is sent
  with `send_message` and acknowledged with `message_sent`/`message_send_failure`; fresh TURN
  credentials are fetched before each call proceeds.
- UI: "📞 Call" in 1:1 chat headers, and a bar above the window for the current call
  (accept/decline, mute, hang up, timer, end reason until closed).

## Not done / unverified

- Never run against Signal: no real call has been placed or received, and the microphone and
  speaker have never been opened. Tests cover the translation, state mapping and UI with fakes.
- Video: incoming video calls are answered audio-only; no camera capture (RingRTC's native
  platform has none; it would need a capture library) and remote video isn't rendered.
- No device picker, no ringtone or desktop notification, no call-history entries
  (`send_call_event`), no group calls. Call messages aren't sent with the `urgent` flag.
- Only Linux x86_64 was tried (the script uses `--platform desktop`, so arm64 may work).
