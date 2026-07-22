# Development

## Commands

```sh
npm run tauri dev       # launch the native development app
npm test                # React unit tests
npm run test:all        # run the complete frontend, Rust, and integration suite
npm run test:integration # native test with a deterministic fixture provider
npm run test:integration:live # native test with Recoll or Spotlight
npm run build           # type-check and build only the web frontend
cargo test --manifest-path src-tauri/Cargo.toml
```

The fixture-backed native integration test uses an embedded WebDriver and a generated video fixture. The live-provider integration test searches for `family vacation` through Recoll on Linux or Spotlight on macOS and expects the indexed `test/fixtures/family_vacation.mp4` file.

## How it works

- `src/` contains the React interface and Tauri command client.
- `src-tauri/` contains the Rust application, search providers, and file-access boundary.
- macOS search runs `/usr/bin/mdfind`.
- Linux search runs `recollq`; installing Recoll through the system package manager normally places it on the app's executable path.
- Linux playback renders libmpv into a GTK OpenGL surface layered over the React player region. React controls it through Tauri commands; other platforms retain the system web media engine.
- Searches are case-insensitive filename keyword searches. All entered words must appear in the filename, and results are sorted, deduplicated, and paginated in groups of 24.
- The frontend receives opaque result IDs rather than filesystem paths. Rust validates the selected file before granting the player temporary asset access.

The `webdriver` Cargo feature enables native automation while retaining the platform search provider. The `e2e` feature additionally replaces that provider with a deterministic fixture provider. Normal development and release builds enable neither feature.

## Linux troubleshooting

- No results: open Recoll, check its indexed folders, then run `recollindex`.
- `recollq` not found: install the `recoll` package and launch Toka from a session where the executable is available on `PATH`.
- Build dependency errors: compare installed packages with the current [Tauri Linux prerequisites](https://v2.tauri.app/start/prerequisites/).
- `libmpv could not be loaded`: install `libmpv2`; on Tails, persist it with Additional Software.
- AppImage compatibility: build on the oldest Linux base you intend to support. Newer build systems may require a newer glibc on the destination computer.

CI verifies the app on macOS arm64 and Ubuntu x64, runs both test suites, and builds the platform bundles defined in `.github/workflows/ci.yml`.
