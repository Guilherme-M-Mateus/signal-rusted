# signal-rusted

A native Signal desktop client written in Rust: an [iced](https://iced.rs) GUI (no web engine)
on top of [presage](https://github.com/whisperfish/presage) and Signal's
[libsignal](https://github.com/signalapp/libsignal).

> **Unofficial.** Not affiliated with or endorsed by Signal Messenger, LLC. "Signal" is their
> trademark. Use at your own risk; for anything sensitive, use the official apps.

## Features

- Link as a secondary device (QR code), with link-time **message history transfer**
- Chat list synced from the storage service (contacts, groups, pins, archive, mute)
- Send/receive 1:1 and group messages, **rich text** (bold, italic, strikethrough, monospace,
  spoilers), **emoji** picker and `:shortcodes:`, mentions, replies, reactions, edits, deletes
- Read/delivery receipts, typing indicators, disappearing messages, message requests
- Attachments with image viewer, link previews, stickers, voice notes (record + play)
- Group management, stories (view + reply), profiles/avatars, settings
- Encrypted local database (SQLCipher) with its key in the OS keyring
- Optional 1:1 voice calls via [RingRTC](https://github.com/signalapp/ringrtc) — see
  [docs/calls.md](docs/calls.md)

## Build

Requires Rust (stable), `protoc`, and a C toolchain.

```sh
cargo run --release          # without calls
cargo test
```

For calls, see [docs/calls.md](docs/calls.md). `packaging/signal-rusted.desktop` and `assets/`
provide a launcher entry and icon.

Data lives in `~/.local/share/signal-rs` (the pre-rename name, kept for existing installs).
`SIGNAL_RS_DATA=/some/dir` runs a separate profile.

## Layout

- `src/backend.rs` — presage on its own thread; commands in, events out
- `src/app.rs`, `src/view.rs` — UI state/update and rendering
- `src/timeline.rs` — folds stored messages (edits, reactions, receipts…) into the timeline
- `src/history.rs` — link-time history import; `src/storage.rs` — storage-service sync
- `vendor/` — presage, vendored with small changes marked `signal-rs:`

## License

AGPL-3.0-only (see [LICENSE](LICENSE)), as required by presage and libsignal.
