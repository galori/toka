# ts: Toka search and indexing as a shell utility

`ts` owns everything the Toka app searches and indexes: the SQLite index,
the search engine, the tags, the thumbnail cache, and the background daemon
that keeps folders current. The app never opens the index itself — it spawns
`ts` for every search, folder change, thumbnail, and status read. This
document is the full `ts` reference; every command, flag, and exit code below
was verified against `ts --help` output and `crates/ts/src/cli.rs`.

## Where ts lives

- Linux installs put `ts` beside the app at `~/.local/opt/toka/ts` and link
  it into `~/.local/bin/ts` (via `ts install --link`).
- macOS bundles ship `ts` inside the app at
  `Toka.app/Contents/MacOS/ts`, next to the `toka` binary. `build:mac:install`
  links it into `~/.local/bin/ts`; a drag-installed bundle has no shell link
  until you run the sidecar's `install --link` once (see below).
- Development and worktree builds keep `ts` beside `toka` under
  `src-tauri/target/<profile>/`.

The app resolves `ts` in this order: the `TS_BIN` environment override, the
`ts` beside the running executable, then `PATH`. Set `TS_BIN` to point a
worktree app at a worktree `ts`.

## Command reference

`ts <command> [arguments]`. Most commands print human-readable output by
default and one JSON document with `--json`; `sync-path`, `sync-rename`
and `daemon run` print no JSON. Paths starting with a dash need
`--` first. `-h`/`--help` after any command shows its help; `-v`/`--version`
prints e.g. `ts 0.1.1+482.a1b2c3d (Git SHA a1b2c3d)`.

Bare `ts`, an unknown command, and bad arguments all print usage to stderr
and exit 2. There is no `ts log` command: read the search log with
`npm run search-log` (or `ts`'s own log files under Files below).

### Search

- `ts search <query> [--json] [--limit N] [--page N] [--type video|image|all] [--count] [--verbose]`
  — one full path per line, sorted by path. `--page N` selects 24-result
  pages, `--limit N` truncates after paging, `--type` keeps only videos,
  images, or all (default), `--count` prints only the match total, and
  `--verbose` (long form only: `-v` is version) streams the per-search trace
  to stderr while stdout keeps one clean path per line. `--type` rejects
  anything but `video`, `image`, `all` with exit 2. A missing index reads
  empty, never created.
- `ts candidates <query> [--json]` — the raw candidate paths behind a query:
  the OR-widened set the engine then filters. This is what the app calls per
  search, applying the viewer's own field and media-type filters itself.
- `ts stats [--json]` — whether the indexer is active, videos/images totals,
  and per-folder state and file counts. Read-only.
- `ts folders list [--json]` — one watched folder per line with its indexing
  state. Read-only.
- `ts folders add <path> [--json]` — watch a folder, as in the app. The
  running daemon picks it up and scans on its own; without a running daemon
  the folder waits for the next start. Prints a confirmation, or the
  resulting folder state with `--json`.
- `ts folders remove <path|id> [--json]` — stop watching a folder; its files
  leave the index. Names the folder by path or by the id
  `ts folders list --json` reports — the id removes folders whose paths are
  not valid UTF-8, which no shell spelling can name.
- `ts thumbnail <path> [--json]` — print one file's thumbnail path,
  generating it on first run; an image prints as its own thumbnail.
- `ts preview <path> [--json]` — print one video's preview frames, one per
  line, generating them on first run; an image prints as its own single
  frame.
- `ts sync-path <path>` — refresh one changed path, as the app does after a
  rename, move, or delete. Prints nothing; a path under no watched folder is
  an error.
- `ts sync-rename <from> <to>` — move one indexed path to its new name, as
  the app does when tagging renames a file or folder. Both paths must sit
  under the same watched folder. Prints nothing.

### Daemon and status

- `ts daemon run` — run the indexer daemon in the foreground: watching every
  search folder, generating thumbnails, and publishing a heartbeat for
  `ts status`. This is the service manager's entry point, not an interactive
  command (which is why `daemon` is absent from the top-level help). One
  supervisor owns each index: a second `ts daemon run` on the same index
  exits 1 instead of starting.
- `ts status [--json] [--watch]` — whether the daemon runs and what it does:
  process, uptime, activity (`idle`, `scanning`, `repairing`, `thumbnails`,
  `offline-waiting`, or `unknown` from a newer daemon), the folder and file
  under the cursor, queued work, and the service's installed/enabled/running
  state. Offline folders list on their own line, never as queued work.
  `--watch` re-renders every second until the daemon is idle with nothing
  queued (Ctrl-C stops it). Exits 3 when the daemon is not running — a watch
  exits 3 at once then — so scripts can branch. Read-only.

Example (idle daemon, no service installed):

```text
Daemon:   running (pid 74252, uptime 11s, heartbeat 1s ago)
Activity: idle
Service:  not installed
```

### Service management

All five take `--json` and print one action message per line otherwise. Every
operation is idempotent and safe to re-run: each step reports what it did
versus the state it already found.

- `ts install [--link] [--json]` — install the indexer as a background
  service for this `ts`: a systemd user unit on Linux, a launchd agent on
  macOS. The service runs `ts daemon run`, starts now, and starts again at
  login. `--link` also links this `ts` into `~/.local/bin`. The last `ts` to
  install wins: installing from another copy repoints the service at itself
  and says so. Run as your own user, never under sudo.
- `ts uninstall [--purge-data] [--json]` — stop and remove the service
  (and the `~/.local/bin/ts` link). `--purge-data` also removes the index,
  the logs, and the thumbnail cache — never media.
- `ts restart [--json]` — stop plus start, so a stopped service ends up
  running and `ts status` uptime resets. The service must be installed first.
- `ts start [--json]` — start the service; starting what already runs
  succeeds and says so. The service must be installed first.
- `ts stop [--json]` — stop the service; stopping what already stopped, or
  was never installed, succeeds and says so.

`ts reset [--yes] [--json]` puts the search state back to first-run: without
`--yes` it lists what would be removed (index, thumbnail cache, log files)
and changes nothing; with `--yes` it stops the indexer first — when the stop
fails, nothing is removed.

### Exit codes

- `0` — ok (also `--help` and `--version`).
- `1` — the operation failed: `ts <command>: <reason>` on stderr. With
  `--json`, stdout holds only the partial documents below; in human mode
  it first prints the completed actions (service operations) or the
  `removed …` lines (`reset --yes`).
- `2` — usage error: usage text on stderr, no document even with `--json`.
- `3` — `ts status` only: the daemon is not running. The report or document
  still prints; the code lets scripts branch.

## The JSON contract

Every `--json` document carries `"schema": 1`. The app checks it on every
response: anything else — a newer schema, malformed JSON, a missing key — is
a version mismatch that surfaces as a human-readable error, never a silent
misread. Documents are pretty-printed, except `ts status --watch --json`,
which streams one compact document per line so each snapshot parses
independently.

Paths in JSON are lossy UTF-8: real media folders can hold non-UTF-8 names,
and those serialize with invalid bytes replaced (U+FFFD), exactly as the
human output renders them. Absent optional paths are JSON null, never an
empty string. The one exception to round-tripping: `ts folders remove`
takes the folder id for paths no shell spelling can name.

Per-command documents:

- `search`: `query`, `page`, `pageSize` (hits this document holds),
  `totalResults`, `totalPages`, `indexRevision`, and `results` — each hit
  with `path`, `id`, `fileName`, `extension`, `thumbnailPath` (null until
  generated), `tags`, and `folderTags`. Without `--page`, `page` is 1 and
  the document holds every hit (truncated by `--limit`); `--count --json`
  prints only `schema`, `query`, and `totalResults`.
- `candidates`: `query`, `indexRevision`, and `candidates` (paths).
- `stats`: `revision`, `videos`, `images`, `serviceRunning`, and `folders`
  (each with `path`, `state`, `scannedFiles`, `videos`, `images`, `message`).
- `status`: `running`, `pid`, `uptimeSecs`, `heartbeatAgeSecs`, `activity`,
  `currentRoot`, `currentPath`, `queue` (`pendingRoots`, `filesRemaining`,
  `pendingRepairs`, `pendingThumbnails`), `offlineRoots`, and
  `serviceInstalled`/`serviceEnabled`/`serviceRunning` (null where the
  service manager cannot answer).
- `folders list`: `folders` (each with `id`, `path`, `status`, `message`,
  `scannedFiles`, `currentPath`, `recentDirectories`).
- `folders add`/`remove --json`: the resulting folder state (`supported`,
  `revision`, `indexedVideos`, `indexedImages`, `indexingFiles`, `folders`).
- `thumbnail`: `thumbnail` (path). `preview`: `previews` (paths, one per
  frame).
- service operations: `command`, `actions` (each with `message` and
  `changed`), and — only on failure — `error`. A failed operation still
  prints its partial actions as one document; the exit is 1.
- `reset --json`: the preview lists `targets`; `--yes` reports `removed`,
  `failures`, and `indexerStopped` (whether the pre-removal stop passed).

## Service management in depth

### systemd (Linux)

`ts install` writes `~/.config/systemd/user/toka-search.service`
(`$XDG_CONFIG_HOME`-aware):

```ini
[Service]
Type=exec
ExecStart="<ts path>" daemon run
Environment="XDG_DATA_HOME=<resolved>"
Environment="XDG_CONFIG_HOME=<resolved>"
Restart=on-failure
RestartSec=2
Nice=10
IOSchedulingClass=idle

[Install]
WantedBy=default.target
```

It then reloads systemd if the file changed, enables and starts the unit,
migrates the legacy `toka-indexer.service` away, attempts
`loginctl enable-linger` so indexing continues while logged out, and — with
`--link` — links `~/.local/bin/ts`. The XDG values pinned in the unit are the
ones the installing shell resolves, defaults included, so the background
daemon watches the same index the CLI uses even when the service manager's
environment differs.

### launchd (macOS)

`ts install` writes `~/Library/LaunchAgents/app.toka.ts.plist` — a
`RunAtLoad` + `KeepAlive` agent of `ProcessType` Background with
`LowPriorityIO`, running `<ts path> daemon run` — and registers it with
`launchctl bootstrap gui/<uid> <plist>`. State is read back through
`launchctl print gui/<uid>/app.toka.ts`: exit 0 means bootstrapped.

No app-sandbox dance is needed: the Toka app ships no sandbox entitlements,
so writing the agent plist and bootstrapping it works directly — from
`ts install` and from the app's first-launch service check alike. The login
item API (`SMAppService`) only enters the picture for sandboxed or App Store
apps; if Toka ever adopts the sandbox, service registration must move to it.
Similarly, no signing is configured in this repository today; the sidecar
ships inside the app bundle, so a future bundle signature covers it (the
bundler signs sidecars with the app).

### Linking

`ts install --link` links the running `ts` into `~/.local/bin/ts`,
creating `~/.local/bin` if needed. The link is attempted even when the
service setup fails, and a link that cannot be created warns instead of
failing an installed service: a real file or a directory in the way is
named but never clobbered. A stale symlink is replaced, and an
already-linked path — or a copy of the binary itself — reports so.
`ts uninstall` removes the link it owns.

`ts install` refuses a temporary copy — an AppImage mount or anything
under the temp directory — since the service would die with it: install
Toka first, then run `ts install` from the installed copy.

## Files

| What            | Linux                                              | macOS                                                             |
| --------------- | -------------------------------------------------- | ----------------------------------------------------------------- |
| Index           | `~/.local/share/toka/index.sqlite3`                | `~/Library/Application Support/toka/index.sqlite3`                |
| Search log      | `~/.local/share/app.toka.desktop/logs/search.log`  | `~/Library/Logs/app.toka.desktop/search.log`                      |
| Indexer log     | `~/.local/share/app.toka.desktop/logs/indexer.log` | `~/Library/Application Support/app.toka.desktop/logs/indexer.log` |
| Thumbnail cache | `$TMPDIR/toka-thumbnails-<uid>`                    | `$TMPDIR/toka-thumbnails-<uid>`                                   |
| Service         | `~/.config/systemd/user/toka-search.service`       | `~/Library/LaunchAgents/app.toka.ts.plist`                        |
| Shell link      | `~/.local/bin/ts`                                  | `~/.local/bin/ts`                                                 |

The Linux data/config paths follow `$XDG_DATA_HOME`/`$XDG_CONFIG_HOME` when
set. Only `ts` opens the index; the app reaches it solely by spawning `ts`.
The plain `$TMPDIR/toka-thumbnails` directory is the orphaned pre-uid-scoping
root: nothing writes there anymore, and reset only removes it.
The daemon rewrites its heartbeat row every 2 seconds; `ts status` counts
the daemon running when its pid is alive and the heartbeat is under
10 seconds old.

## Troubleshooting

- **App reports it cannot find `ts`.** The helper is missing from beside the
  app and from `PATH`. Reinstall the app, or point the app at a copy with
  `TS_BIN=/path/to/ts`. A worktree app needs a worktree `ts` (`cargo build
-p ts`) or a `TS_BIN` override.
- **App reports it disagrees with `ts`, or an unknown schema.** The app and
  the helper are from different checkouts: `ts --version` against the app's
  build version (About screen) shows which is stale. Reinstall, or repoint
  `TS_BIN`. Dev and worktree mixes keep working as long as both sides speak
  `"schema": 1`.
- **Daemon not running (`ts status` exits 3).** Start it with `ts start`
  (after `ts install`), or run `ts daemon run` in a terminal to watch it in
  the foreground. If the service is installed but the daemon keeps dying,
  read the indexer log above.
- **`ts install` fails under sudo.** Install as your own user: under sudo
  the service would be registered for root instead of you.
- **`ts install --link` warns about a file in the way.** `~/.local/bin/ts`
  is a real file `ts` will not clobber — the service still installs, and
  moving the file aside plus a re-run adds the link.
- **`ts install` refuses a temporary copy.** The `ts` runs from an AppImage
  mount or under the temp directory, where a service registration would die
  with it. Install Toka first, then run `ts install` from the installed
  copy.
- **Service points at the wrong copy.** The last `ts` to install wins; run
  `~/.local/bin/ts install` (or the copy you want) to take it over — it
  says so when it does.
- **Indexing stops at logout (Linux).** `ts install` attempts
  `loginctl enable-linger`; if that failed (see its report), enable it with
  `loginctl enable-linger "$USER"`.
- **Search finds nothing new.** Check `ts status` (is the daemon scanning?)
  and `ts folders list` (is the folder watched and ready?). A disconnected
  drive reports its folders offline and keeps their existing rows.
