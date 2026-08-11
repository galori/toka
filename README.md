# Toka

Toka is a small desktop app for finding and playing videos on macOS and Linux. On Linux it maintains a private search index for the folders you choose; on macOS it uses Spotlight. It never changes the contents of your files except when you edit a video's tags, which live in its filename.

## Quick start

- [Linux (Ubuntu/Debian)](docs/linux-quick-start.md)
- [macOS](docs/macos-quick-start.md)

## Build

```sh
npm run build:linux # Linux: builds and installs for the current user
npm run build:mac   # macOS: builds the .app and .dmg
```

Build Linux artifacts on Linux and macOS bundles on macOS. The Linux command installs
Toka, its private indexer, and its bundled plocate tools under your home directory,
so no system-wide plocate setup is needed. See the platform quick-start guide for details.

## Search

A search looks at three separate things, and any combination of them can be
switched on: a video's tags (`Ctrl+T`), its filename without the tag block
(`Ctrl+F`), and the folders above it (`Ctrl+P`). Tags and the filename are on to
begin with. Each search term has to be found in one of the parts being searched,
so a second word still narrows a search even when it lands somewhere else.

Changing what is searched runs the search again straight away. The last part
left on cannot be switched off, because a search with nothing to look at could
only ever answer "no videos". The choice lasts as long as Toka is open, like
every other setting here.

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
- On Linux, `Ctrl+,` opens the search-folder screen. Toka updates those private indexes after files are created, removed, moved, or renamed, even while its window is closed.
- A disconnected external folder is shown as offline. Its index is retained and incrementally refreshed when the same drive returns at the same mount path.

See [Development](docs/development.md) for tests, project structure, troubleshooting, and implementation details. The official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) cover other Linux distributions.
