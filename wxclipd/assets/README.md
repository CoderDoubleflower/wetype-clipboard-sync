# WeType icon provenance

`wetype-source.png` is the authentic 微信输入法 / WeType icon published by
**Tencent Technology (Shenzhen) Company Ltd.** for Android package
`com.tencent.wetype`. It was retrieved on 2026-10-21 and visually checked.

- Publisher listing: <https://play.google.com/store/apps/details?hl=en&id=com.tencent.wetype>
- Original image: <https://play-lh.googleusercontent.com/FaJwxdcJv44bzUt-Bcz1cjxMD109BaFtLM6UjDI2rN6_LeXCvclaD1iRVcEiFYyqmQw2Iju92n3XjyOu6G-l=w512-h512>
- Original PNG SHA-256: `d45a627ea09b254e2c0d3b25b3da24d2c850486a6676f23fb9d8f699470412fc`
- Windows ICO SHA-256: `b9d8b57a86316ae2c445139f2f075d7b955b75098ab01e86b6cf778185b2eeb4`

`wetype.ico` contains 16, 24, 32, 48, 64, 128 and 256 pixel renditions. Only
resizing and conversion to Windows ICO were performed; the artwork was not
redrawn. To regenerate with ImageMagick:

```sh
convert wetype-source.png -define icon:auto-resize=256,128,64,48,32,24,16 wetype.ico
```

The WeType name and icon belong to their respective Tencent rights holders.
These assets are **not covered by this repository's MIT license**, and their
inclusion does not grant trademark rights or imply Tencent endorsement. This
repository is an independent, unofficial clipboard synchronization client.

The icon is included at the repository owner's request for identifying this
client's Windows executables, installer, shortcuts and notification-area entry.
The package retains this notice under `licenses/wetype-icon-NOTICE.md`.
