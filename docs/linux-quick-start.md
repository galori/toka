# Linux quick start

These instructions target Ubuntu and Debian. For another distribution, install the equivalent packages from the official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

## 1. Install prerequisites

```sh
sudo apt update
sudo apt install build-essential curl file libayatana-appindicator3-dev \
  libmpv2 librsvg2-dev libssl-dev libwebkit2gtk-4.1-dev libxdo-dev recoll wget
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

To build the `.deb` and install it in one step:

```sh
npm run build:install
```

This writes the package below `src-tauri/target/release/bundle/deb/` and installs
it to add Toka to the Applications menu. `npm run tauri build` only builds the
package without installing it. The install uses `dpkg` directly because Tails'
Additional Software hook rejects locally built packages passed through `apt`.

On Tails, the repository and package remain available because they are in
Persistent Storage, but the installed application does not survive a reboot.
Run `npm run build:install` after rebooting with Persistent Storage unlocked,
or add the install command to an idempotent persistent setup script.

Toka uses `libmpv2` for Linux video playback. On Tails, add `libmpv2` to
Additional Software so the playback library is restored when Persistent
Storage is unlocked. Installing Celluloid also supplies this library, but Toka
does not launch or otherwise depend on the Celluloid application.

If searches return nothing, check Recoll's indexed folders and run `recollindex` again. More troubleshooting is available in [Development](development.md).
