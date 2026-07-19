# macOS quick start

## 1. Install prerequisites

Install Apple's command-line tools:

```sh
xcode-select --install
```

Skip that command if they are already installed. Install [Node.js 20+](https://nodejs.org/) and [Rust stable](https://rustup.rs/) if needed.

## 2. Install and run Toka

From the repository:

```sh
npm ci
npm run tauri dev
```

Toka searches the existing Spotlight index; no separate search service is required.

## 3. Build an installable app

```sh
npm run tauri build
```

The `.app` and `.dmg` bundles are written below `src-tauri/target/release/bundle/`.

If searches omit expected files, check Spotlight exclusions and Toka's access under **System Settings → Privacy & Security**.
