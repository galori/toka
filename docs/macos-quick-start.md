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

Toka searches its own index of the folders you choose — no Spotlight index
required. Open Search folders with `Ctrl+,` to choose folders. A `ts`
background service (a launchd agent) indexes those folders, and keeps going
after Toka quits. [ts.md](ts.md) covers the service and the `ts` command
reference.

## 3. Build an installable app

```sh
npm run build:mac:install
```

The `.app` and `.dmg` bundles are written below `src-tauri/target/release/bundle/`.
The app ships its `ts` search helper inside the bundle at
`Toka.app/Contents/MacOS/ts`, next to the `toka` binary.
`build:mac:install` also installs the app in `/Applications`, links the `toka`
command at `~/.local/bin/toka`, and runs the bundled `ts install --link`:
that points the launchd agent at the installed copy, links `ts` into
`~/.local/bin/ts`, and restarts the daemon onto it. Add that folder to your
`PATH` if your shell does not find the commands:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Add this line to `~/.zshrc` to keep the change after you close the terminal.

If you install the app from the `.dmg` by drag-and-drop, no install script
runs: the app registers the launchd agent itself on first launch, but the
shell links are yours to create after you move Toka to `/Applications`:

```sh
mkdir -p "$HOME/.local/bin"
ln -s "/Applications/Toka.app/Contents/MacOS/toka" "$HOME/.local/bin/toka"
ln -s "/Applications/Toka.app/Contents/MacOS/ts" "$HOME/.local/bin/ts"
```

Then search from a terminal with `toka search "family trip"`. Toka prints each
matching video or image path on a separate line. The command uses the same
query syntax as the app.

If searches omit expected files, check the folders and indexing status under Search folders (`Ctrl+,`), then check Toka's access under **System Settings → Privacy & Security**.
