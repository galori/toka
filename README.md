# Toka

Toka is a small desktop app for finding and playing videos on macOS and Linux. It searches the operating system's existing index—Spotlight on macOS or plocate on Linux. It never changes the contents of your files; the only thing it writes is a video's tags, which live in its filename.

## Quick start

- [Linux (Ubuntu/Debian)](docs/linux-quick-start.md)
- [macOS](docs/macos-quick-start.md)

## Build

```sh
npm run build:linux # Linux: builds and installs the .deb
npm run build:mac   # macOS: builds the .app and .dmg
```

Build Linux bundles on Linux and macOS bundles on macOS. The Linux command installs
the generated `.deb` so Toka appears in the Applications menu and can be launched
from there. See the platform quick-start guide for details.

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
- Search results are based on the current Spotlight or plocate index, not a live filesystem scan.

See [Development](docs/development.md) for tests, project structure, troubleshooting, and implementation details. The official [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) cover other Linux distributions.
