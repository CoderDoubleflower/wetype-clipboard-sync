# Vendored: wx-ime-sdk

- Upstream: https://github.com/mkdir700/wx-ime-sdk
- Commit: 1ec70393cc9524c6c341cf768f090b0176586f72
- Vendored at: 2026-10-12
- License: Apache-2.0, see `LICENSE`
- Local changes: none yet, keep this list in sync when patching

This tree is the protocol layer (`crates/wx-ime-protocol`) and async client
(`crates/wx-ime-sdk`) for the WeType (微信输入法) cross-device clipboard. The
`wxclipd/` crate in this repository adds the Linux desktop integration that the
upstream SDK deliberately leaves out.
