# Toka

Toka is a small desktop app for finding and playing videos on macOS and Linux. On both platforms it maintains a private search index for the folders you choose. It never changes the contents of your files except when you edit a video's tags, which live in its filename.

## Quick start

- [Linux (Ubuntu/Debian)](docs/linux-quick-start.md)
- [macOS](docs/macos-quick-start.md)

## Build

```sh
npm run build:linux         # Linux: builds only, prints the binary path
npm run build:linux:install # Linux: builds and installs for the current user
npm run build:mac           # macOS: builds the .app and .dmg only
npm run build:mac:install   # macOS: builds, then installs into /Applications
```

Build Linux artifacts on Linux and macOS bundles on macOS. Only the `:install` variants install
anything. The Linux install puts Toka and its private indexer under your home directory,
so no system-wide setup is needed. See the platform quick-start guide for details.
The macOS install puts the app in `/Applications` and links the `toka` command
in `~/.local/bin`.

## Releases

Published builds are on the [Releases page](https://github.com/galori/toka/releases):

- macOS (Apple Silicon): the `.dmg`, or the `.app.tar.gz` when you prefer to
  unpack the app yourself.
- Ubuntu/Debian: the `.deb`, or the portable `.AppImage`. The `.deb` installs
  its playback dependency automatically; the `.AppImage` needs `libmpv2` on
  the host (`sudo apt install libmpv2`).

The builds are unsigned. On first launch macOS Gatekeeper may refuse to open
Toka. On macOS Sequoia, Control-click does not override Gatekeeper for
unsigned apps. Instead:

1. Try to open Toka, then dismiss the warning.
2. Open System Settings > Privacy & Security, scroll to the Security section,
   and click Open Anyway.
3. Open Toka again and confirm. Enter your admin password if prompted.

On earlier macOS versions, right-click (or Control-click) the app, choose
Open, then confirm.

Every merge to `main` publishes a release automatically once CI is green:
the release workflow builds both platform bundles and attaches them to a new
release tagged `v<major>.<minor>.<main commit count>` (for example `v0.1.250`),
so release versions grow monotonically and re-running a release rebuilds the
same version. The manifests carry only the base major.minor; the workflow
stamps the full version into its own checkout and never commits it.

## Search

A search looks at three separate things, and any combination of them can be
switched on: a video's tags (`Ctrl+T`), its filename without the tag block
(`Ctrl+F`), and the folders above it (`Ctrl+P`). Tags and the filename are on to
begin with. Each search term has to be found in one of the parts being searched,
so a second word still narrows a search even when it lands somewhere else.

All three parts are on to begin with. Changing what is searched runs the search
again straight away. The last part left on cannot be switched off, because a
search with nothing to look at could only ever answer "no videos". The choice
lasts as long as Toka is open, like every other setting here.

Searches accept implicit `AND`, explicit `AND`, `OR`, and `NOT`. `AND` binds
more tightly than `OR`, while `NOT` binds most tightly. Parentheses change the
order. Quoted values keep spaces, such as `filename:"family trip"`.

Text can target `tags:`, `filename:`, `path:`, or `folder:`. `folder:`
searches every parent path component; `path:` searches the whole path including
the file's own name, so `path:/Videos/trip` finds everything under that folder
and `path:/Videos/trip/clip.mp4` finds the one file — even where the index has
never looked. Quote values with spaces, such as `path:"/My Videos/trip"`.
`type:image` and `type:video` match media kinds. `extension:jpg` matches an
exact extension without regard to case.

File-size predicates can be added to a search without a separate control:
`size:18mb` matches 18.0 MB up to (not including) 19 MB, and `size:36.3mb`
matches 36.3 up to 36.4 MB: a bare size covers one step of its last digit. A
size in plain bytes matches exactly. Operators express ranges, such as
`size:<1.5gb` and `size:>=100kb`. `kb`, `mb`, `gb`, and `tb` use decimal units;
the corresponding `kib`, `mib`, `gib`, and `tib` suffixes use binary units.

Creation and modification dates can be searched with the same comparison
operators: `created:10-5-2026`, `modified:>1-1-2026`, and
`created:<=12-10-2025`. Dates accept `MM-DD-YYYY`, `MM-DD-YY`, and
`YYYY-MM-DD`; one- or two-digit months and days are accepted, and two-digit
years use 00–68 for 2000–2068 and 69–99 for 1969–1999. Quoted relative values
such as `created:">= 1 day ago"`, `modified:">= 30 minutes ago"`, and
`modified:">= last 30 minutes"` resolve once when the search starts and compare
the exact file timestamp, so a one-day window means the last 24 hours. Relative
values accept seconds, minutes, hours, days, and weeks, plus compact units such
as `30m ago`. An unqualified day or week value such as `modified:"3 days ago"`
matches that local calendar day; explicit calendar dates use the same local-day
matching.

Results arrive shuffled, so a broad search stays a mix. The sort control
beside the results orders them instead: by name, size, or creation,
modification, or access date, ascending or descending. `Ctrl+S` steps
through the orders and `Ctrl+Shift+S` turns the order around. The order
holds across every page of the search.

### Search from the command line

Run a search from a terminal with `toka search`:

```sh
toka search "family trip"
```

The command uses Toka's index and the same query syntax. It searches videos and
images, then writes each full file path on a separate line. It returns exit code
`0` when the search succeeds, including when it finds no results. A search or
output error returns exit code `1`. A missing query returns exit code `2`. Use
`toka search --help` for command help.

`toka --help` (or `-h`) lists the available commands, and `toka --version` (or
`-v`) prints the installed Toka version and the Git SHA it was built from
(the same SHA shown when the app first launches), e.g.
`toka <version> (Git SHA 1a2b3c…)`.

Run `toka` with a folder, video, or playlist to play it straight away:

```sh
toka ~/Videos/trip
toka ~/Videos/trip/clip.mp4
```

The folder or file opens playing, with its `path:` query in the search field,
so the results stay a search that can be asked again. The same opening is on
the search screen as Open folder (`Ctrl+Shift+P`) and Open file
(`Ctrl+Shift+F`).

### Reset Toka

`toka reset --yes` puts Toka back to a first-run state. It removes every search
folder, the search index and its caches, the thumbnail and other caches, and
Toka's log files (including verbose search logs saved to your chosen folder).
It never touches your media files, tags, or add-ons. On Linux it also stops the
background indexer; the next time you open Toka it starts again with no folders.
Without `--yes`, `toka reset` only lists what it would remove and changes
nothing. Close Toka before resetting.

### Restart Toka

`toka restart` (Linux) restarts the Toka app and its indexer. It only affects
the Toka you called: the installed `toka` restarts the installed app, while a
`toka` built in a worktree restarts only its own app and leaves the installed
one alone (and the other way round). Either way the indexer is the shared
`ts` service, restarted with `ts restart`.

## Add-ons

While a video plays, press `Ctrl+Shift+A` to open actions supplied by add-ons.
The bundled TikTok add-on recognizes names such as
`2026-10-10_@sometiktoker_18938383490812348343.mp4`. Its actions can open a new
Toka window containing only that exact creator's videos, or open the creator
page in the default browser. The Add-ons control stays disabled when the
current filename matches no action.

## Playlists

Every page of results is a playlist: playing one video plays the rest after it,
with `PageUp` and `PageDown` for the previous and next entry. Later search pages
are fetched as playback reaches them, so a large result set does not need to be
loaded into the player all at once.

Toka can also be launched with a playlist file, and starts playing it straight
away:

```sh
toka ~/Videos/summer.m3u8
```

Opening a `.m3u8` or `.m3u` file with Toka from a file manager does the same
thing. The format is the extended M3U that the `Open in` control writes for other
players: one path per line, `#` lines ignored, and a path written relative to the
playlist resolved against the folder the playlist is in. Entries that have since
been deleted are skipped rather than stopping the playlist, and the playlist's
entries stay on screen as the list to come back to.

## Tagging

Tags are stored in the filename, inside square brackets before the extension:

```
my_home_video [cute home].mp4
```

Multiple tags are space-separated, lowercased, sorted alphabetically, and never
duplicated. Press `T` on a video to add a tag, or click a tag to remove it —
Toka renames the file to match, and refuses the change if that name is already
taken.

## Notes

- Supported search results: MP4, MOV, MKV, AVI, WebM, M4V, MPEG, MPG, and MPE.
- Linux playback uses the embedded libmpv/FFmpeg media engine; other platforms use the system WebKit media engine. Toka does not transcode files.
- On both platforms, `Ctrl+,` opens the search-folder screen. Both search Toka's private SQLite index. On Linux, Toka updates the private index after files are created, removed, moved, or renamed, even while its window is closed.
- A disconnected external folder is shown as offline. Its index is retained and incrementally refreshed when the same drive returns at the same mount path.

See [Development](docs/development.md) for tests, project structure, troubleshooting, and implementation details. The official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) cover other Linux distributions.
