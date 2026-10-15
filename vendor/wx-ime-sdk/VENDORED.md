# Vendored: wx-ime-sdk

- Upstream: https://github.com/mkdir700/wx-ime-sdk
- Commit: 1ec70393cc9524c6c341cf768f090b0176586f72
- Vendored at: 2026-10-12
- License: Apache-2.0, see `LICENSE`
- Local changes in this fork (keep this list in sync when patching):
  - `crates/wx-ime-protocol/proto/wetype.proto`: add `UnbindRequest` / `UnbindResponse`
    (command 35; field numbers read from the official 4.0.0 RN bundle decoder).
  - `crates/wx-ime-protocol/src/transport.rs`: add `UNBIND = 35`.
  - `crates/wx-ime-sdk/src/session.rs`: `group_with_version()` so callers can advertise
    the clipboard revision they already hold.
  - `crates/wx-ime-sdk/src/storage.rs`: `State::known_clipboard_version()`.
  - `crates/wx-ime-sdk/src/clipboard/receive.rs`: poll with that revision instead of 0,
    so the server stops re-sending unchanged clipboard payloads.
  - `crates/wx-ime-sdk/src/client.rs`: public `group_with_clipboard_version()` and
    `unbind()`.

This tree is the protocol layer (`crates/wx-ime-protocol`) and async client
(`crates/wx-ime-sdk`) for the WeType (微信输入法) cross-device clipboard. The
`wxclipd/` crate in this repository adds the Linux desktop integration that the
upstream SDK deliberately leaves out.
