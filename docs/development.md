# Development

## Commands

```sh
npm run tauri dev       # launch the native development app
npm test                # React unit tests
npm run test:integration # build, launch, and interact with the native app
npm run build           # type-check and build only the web frontend
cargo test --manifest-path src-tauri/Cargo.toml
```

The native integration test uses an embedded WebDriver and a generated video fixture. It does not require a populated Spotlight or plocate index.

## How it works

- `src/` contains the React interface and Tauri command client.
- `src-tauri/` contains the Rust application, search providers, and file-access boundary.
- macOS search runs `/usr/bin/mdfind`.
- Linux search uses `plocate` by default. Set `TOKA_SEARCH_PROVIDER=recoll` before launching Toka to use `recollq` instead.
- Linux playback renders libmpv into a GTK OpenGL surface layered over the React player region; other platforms retain the web media engine.
- Searches are case-insensitive filename keyword searches. All entered words must appear in the filename, and results are sorted, deduplicated, and paginated in groups of 24.
- The frontend receives opaque result IDs rather than filesystem paths. Rust validates the selected file before granting the player temporary asset access.
- Subtitles come from sidecar files beside the video (`talk.srt`, `talk.en.srt`) and from tracks inside it. Rust detects the sidecars and converts SRT to WebVTT for the web media engine; on Linux mpv supplies both sidecar and embedded tracks. `S` toggles them.

`docs/thumbnails.md` records the decided approach for result thumbnails.

The `e2e` Cargo feature replaces the platform search provider with a fixture provider and enables the WebDriver plugins. Normal development and release builds do not enable it.

## Linux troubleshooting

- No results: check `/etc/updatedb.conf`, then run `sudo updatedb` to refresh plocate's index.
- `plocate` not found: install the `plocate` package and launch Toka from a session where the executable is available on `PATH`.
- Optional Recoll provider: install `recoll`, run `recollindex`, and launch Toka with `TOKA_SEARCH_PROVIDER=recoll`.
- Build dependency errors: compare installed packages with the current [Tauri Linux prerequisites](https://v2.tauri.app/start/prerequisites/).
- `libmpv could not be loaded`: install `libmpv2`; on Tails, persist it with Additional Software.
- AppImage compatibility: build on the oldest Linux base you intend to support. Newer build systems may require a newer glibc on the destination computer.

CI verifies the app on macOS arm64 and Ubuntu x64, runs both test suites, and builds the platform bundles defined in `.github/workflows/ci.yml`.
