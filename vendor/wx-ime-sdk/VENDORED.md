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
  - `crates/wx-ime-protocol/proto/websocket.proto` + `build.rs` + `lib.rs`: push-channel
    frames (`WebSocketPbRequest/Response`, `HttpRequest/Response`, `WebSocketPushInfo`),
    field numbers from Ghidra on the 4.0.0 native library.
  - `crates/wx-ime-sdk/src/notify.rs`: WebSocket push channel — connect with the
    `wxws_pb` subprotocol, redo ECDH on the socket, subscribe with cmd 8074 and decode
    pushed events. `Cargo.toml` gains `tokio-tungstenite` + `futures-util`.
  - `crates/wx-ime-sdk/src/network.rs` + `client.rs` + `session.rs` +
    `clipboard/cos.rs` + `notify.rs`: explicit `ClientOptions::proxy` shared by
    control HTTP, COS (including text larger than 256 KiB) and WebSocket traffic.
    Supports HTTP CONNECT and SOCKS5/SOCKS5h with username/password authentication,
    consistent local/remote DNS, bounded handshakes, normal destination TLS
    validation, redacted diagnostics and no direct fallback on proxy failure.
    `Cargo.toml` enables reqwest's `socks` feature and adds the tunnel dependencies;
    loopback tests cover routing, authentication, DNS and failure handling.

This tree is the protocol layer (`crates/wx-ime-protocol`) and async client
(`crates/wx-ime-sdk`) for the WeType (微信输入法) cross-device clipboard. The
`wxclipd/` crate in this repository adds the Linux desktop integration that the
upstream SDK deliberately leaves out.
