# Toka

Toka is a small, read-only desktop app for finding and playing videos on macOS and Linux. It searches the operating system's existing index—Spotlight on macOS or Recoll on Linux—and never moves or modifies your files.

## Quick start

- [Linux (Ubuntu/Debian)](docs/linux-quick-start.md)
- [macOS](docs/macos-quick-start.md)

## Build

```sh
npm run build:install
```

This builds the Tails `.deb` package and installs it so Toka appears in the
Applications menu. Use `npm run tauri build` when you only want to build the
package; see the [Linux quick start](docs/linux-quick-start.md).

## Notes

- Supported search results: MP4, MOV, MKV, AVI, WebM, M4V, MPEG, MPG, and MPE.
- Linux playback uses the embedded libmpv/FFmpeg media engine. Toka does not transcode files.
- Search results are based on the current Spotlight or Recoll index, not a live filesystem scan.

See [Development](docs/development.md) for tests, project structure, troubleshooting, and implementation details. The official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) cover other Linux distributions.
