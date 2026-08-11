# Linux quick start

These instructions target Ubuntu and Debian. For another distribution, install the equivalent packages from the official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

## 1. Install prerequisites

```sh
sudo apt update
sudo apt install build-essential curl file libayatana-appindicator3-dev \
  libegl1-mesa-dev libepoxy-dev libglx-dev libgtk-3-dev libmpv2 librsvg2-dev \
  libssl-dev libwebkit2gtk-4.1-dev libxdo-dev libzstd-dev meson ninja-build wget
```

Install [Node.js 20+](https://nodejs.org/) and [Rust stable](https://rustup.rs/) if they are not already installed.

## 2. Install and run Toka

From the repository:

```sh
npm ci
node scripts/build-plocate.mjs
npm run tauri dev -- --config src-tauri/tauri.linux-owned-index.conf.json
```

Toka opens its search-folder screen on first launch. Add one or more media folders;
the initial index starts immediately and a per-user background service keeps it
current after the Toka window closes. Use `Ctrl+,` to change the folders later.

## 3. Build an installable app

```sh
npm run build:linux
```

This installs Toka into `~/.local/opt/toka` and registers a desktop entry, so it
appears in the Applications menu and can be launched from there. No `sudo` is
needed, and nothing outside your home directory is touched. It also links the
`toka` command into `~/.local/bin` and enables `toka-indexer.service` for the
current user. The service runs while you are logged in; it does not enable
systemd lingering.

Add `-- --appimage` to build and install a self-contained AppImage instead. That
is what you want when copying Toka to another machine; it takes about 90 seconds
longer, because it bundles GTK and WebKit alongside the app.

Toka uses `libmpv2` for Linux video playback. On Tails, add it to Additional Software so it is restored when Persistent Storage is unlocked.

If searches return nothing, open Search folders with `Ctrl+,` and check each
folder's status. More troubleshooting is available in [Development](development.md).
