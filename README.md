# Photon

A fast, lightweight, native terminal-first editor: Warp-style terminal, Zed's editor, AI-assisted PR
review through the coding agents you already use (Claude Code, Devin, and other ACP agents), and no
account or hosted service required.

Photon is a fork of [Zed](https://github.com/zed-industries/zed) by Zed Industries, licensed GPL-3.0.

### Building

Prerequisites (macOS): Xcode, rustup, and `brew install cmake`. See
[Building Zed for macOS](./docs/src/development/macos.md) for details.

```sh
cargo run -p zed            # debug build
script/bundle-mac           # Photon.app bundle
```

Design docs live in [`docs/photon/`](./docs/photon/).

### Licensing

Photon source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where
marked, the same as the Zed source it derives from.

License information for third party dependencies is checked with
[`cargo-about`](https://github.com/EmbarkStudios/cargo-about). New crates need `publish = false`
under `[package]`; accepted licenses are listed in `script/licenses/zed-licenses.toml`.
