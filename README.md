# Toka

Toka is a small, read-only desktop video search and player for macOS and Linux. It uses the operating system's existing search index: Spotlight through `mdfind` on macOS and Recoll through `recollq` on Linux.

## Development

Prerequisites:

- Node.js 20 or newer
- Rust stable
- The [Tauri 2 platform prerequisites](https://v2.tauri.app/start/prerequisites/)

Install dependencies and start the native development app:

```sh
npm install
npm run tauri dev
```

Run the complete local checks:

```sh
npm test
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
```

## Search providers

### macOS

Toka uses `/usr/bin/mdfind`, so results depend on Spotlight's current index and the folders macOS permits Toka to access. If expected files are absent, check Spotlight indexing and macOS Privacy & Security settings.

### Ubuntu/Debian Linux

Install Recoll and create its index before using Toka:

```sh
sudo apt install recoll
recollindex
```

Update the index with `recollindex` after adding or renaming videos. Toka searches the folders configured in Recoll; it does not create or manage that configuration.

## Playback support

Search results include MP4, MOV, MKV, AVI, WebM, M4V, MPEG, MPG, and MPE files. Playback uses the system WebKit media engine, so a listed container or codec may not be playable. Toka reports that failure without modifying or transcoding the original file.

Build artifacts are produced by `npm run tauri build`. CI builds macOS arm64 and Ubuntu/Debian x64 bundles, including Linux AppImage and `.deb` artifacts where supported by Tauri.
