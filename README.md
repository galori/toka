# Toka

Toka is a small, read-only desktop app for finding and playing videos on macOS and Linux. It searches the operating system's existing index—Spotlight on macOS or Recoll on Linux—and never moves or modifies your files.

## Quick start

- [Linux (Ubuntu/Debian)](docs/linux-quick-start.md)
- [macOS](docs/macos-quick-start.md)

## Build

```sh
npm run tauri build
```

Installable bundles are written below `src-tauri/target/release/bundle/` (`.deb` and AppImage on Linux; `.app` and `.dmg` on macOS). Build Linux bundles on Linux and macOS bundles on macOS.

## Notes

- Supported search results: MP4, MOV, MKV, AVI, WebM, M4V, MPEG, MPG, and MPE.
- Linux playback uses the embedded libmpv/FFmpeg media engine; other platforms use the system WebKit media engine. Toka does not transcode files.
- Search results are based on the current Spotlight or Recoll index, not a live filesystem scan.

See [Development](docs/development.md) for tests, project structure, troubleshooting, and implementation details. The official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) cover other Linux distributions.
