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

Toka searches the existing Spotlight index; no separate search service is required. Open Search folders with `Ctrl+,` to limit searches to the folders you choose. Toka also maintains its own index of those folders in the background.

## 3. Build an installable app

```sh
npm run build:mac
```

The `.app` and `.dmg` bundles are written below `src-tauri/target/release/bundle/`.
The build installs the app in `/Applications` and links the `toka` command at
`~/.local/bin/toka`. Add that folder to your `PATH` if your shell does not find
the command:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Add this line to `~/.zshrc` to keep the change after you close the terminal.

If you install the app from the `.dmg` and no `toka` link exists, create the
command link after you move Toka to `/Applications`:

```sh
mkdir -p "$HOME/.local/bin"
ln -s "/Applications/Toka.app/Contents/MacOS/toka" "$HOME/.local/bin/toka"
```

Then search from a terminal with `toka search "family trip"`. Toka prints each
matching video or image path on a separate line. The command uses the same
query syntax as the app.

If searches omit expected files, check the folders under Search folders (`Ctrl+,`), then check Spotlight exclusions and Toka's access under **System Settings → Privacy & Security**.
