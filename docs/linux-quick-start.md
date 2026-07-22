# Linux quick start

These instructions target Ubuntu and Debian. For another distribution, install the equivalent packages from the official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

## 1. Install prerequisites

```sh
sudo apt update
sudo apt install build-essential curl file libayatana-appindicator3-dev \
  libegl1-mesa-dev libepoxy-dev libglx-dev libgtk-3-dev libmpv2 librsvg2-dev \
  libssl-dev libwebkit2gtk-4.1-dev libxdo-dev recoll wget
```

Install [Node.js 20+](https://nodejs.org/) and [Rust stable](https://rustup.rs/) if they are not already installed.

## 2. Install and run Toka

From the repository:

```sh
npm ci
recollindex
npm run tauri dev
```

`recollindex` creates or refreshes Toka's search index. Use the Recoll desktop app to choose which folders are indexed.

## 3. Build an installable app

```sh
npm run build:linux
```

This builds the `.deb` below `src-tauri/target/release/bundle/deb/` and installs it
with `dpkg` so Toka appears in the Applications menu and can be launched from there.

Toka uses `libmpv2` for Linux video playback. On Tails, add it to Additional Software so it is restored when Persistent Storage is unlocked.

If searches return nothing, check Recoll's indexed folders and run `recollindex` again. More troubleshooting is available in [Development](development.md).
