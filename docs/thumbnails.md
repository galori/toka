# Thumbnail approach

Recommendation for how Toka should display thumbnails for search results, decided
before implementation (GitHub issue #23).

## Recommendation

Use a **shared-cache hybrid**: read from the freedesktop thumbnail cache first,
and when an entry is missing, generate one and **write it back into that same
cache in the standard format**. Toka then never blocks on Nautilus having visited
a folder, and every thumbnail Toka produces is picked up by Nautilus, Files,
GTK file choosers, and any other freedesktop-compliant application.

This directly answers the open question on the issue: yes, Toka-generated
thumbnails can be stored so that the rest of the desktop benefits from them.

## Why the shared cache works

GNOME does not own the cache; it implements the freedesktop.org *Thumbnail
Managing Standard*, which any application may read and write. Verified against
this machine's cache:

- Location: `$XDG_CACHE_HOME/thumbnails/<size>/`, where `<size>` is one of
  `normal` (128px), `large` (256px), `x-large` (512px), `xx-large` (1024px).
  The number is the maximum of width and height; aspect ratio is preserved.
- File name: the lowercase hex MD5 of the file's canonical URI, plus `.png`.
  The URI is the percent-encoded `file://` form, exactly as `g_filename_to_uri`
  produces it — hashing a differently-escaped string yields a name no other
  application will look up.
- Format: PNG with `tEXt` chunks `Thumb::URI` (that same URI) and `Thumb::MTime`
  (the source file's mtime in whole seconds, as a decimal string). Readers treat
  a thumbnail whose `Thumb::MTime` differs from the source as stale and
  regenerate it, which is also how Toka gets free invalidation.
- Failures are recorded as a zero-size PNG under
  `$XDG_CACHE_HOME/thumbnails/fail/<application>/<md5>.png`, so a file that
  cannot be decoded is not retried on every browse.

Writing rules that matter for interoperability:

- Create the file as `0600` in a temporary name inside the destination directory,
  then `rename(2)` it into place, so readers never observe a partial PNG.
- Directories are `0700`.
- Write `Software` as `Toka` so the origin of an entry is identifiable.

## Generating the missing entries

Toka should not implement video decoding for this. The desktop already ships
per-MIME-type thumbnailer programs in `/usr/share/thumbnailers/*.thumbnailer`
(`gst-video-thumbnailer` and `ffmpegthumbnailer` cover the video types Toka
searches for). Each entry declares an `Exec` line with `%u` (input URI), `%o`
(output path), and `%s` (size in pixels). Invoking those is exactly what Nautilus
does, so the resulting images are byte-comparable with what the file manager
would have produced, and the visual result stays consistent across the desktop.

Generation should be:

- **Lazy and viewport-driven** — request thumbnails for the result tiles actually
  on screen, not for a folder tree. A search page is 24 results, which bounds the
  work naturally and sidesteps the "generate for an entire tree" problem
  described on the issue.
- **Bounded and cancellable** — a small worker pool, with requests dropped when
  the user scrolls away or starts a new search.
- **Low priority** — the generator subprocess runs niced, with a per-file
  timeout, and a timeout records a `fail/` entry rather than retrying.

## Rejected alternatives

- **Read the system cache only.** This is what the issue describes going wrong:
  entries exist only for folders already opened in Files, so most results show a
  placeholder forever, and there is no supported way to make Nautilus populate a
  tree on demand.
- **Toka-private cache only.** Reliable, but duplicates work the desktop has
  often already done, doubles disk usage, and gives nothing back to Nautilus —
  which the issue explicitly asks for.

## macOS

There is no shared cache to contribute to. Toka keeps a private cache under
`~/Library/Caches/com.toka.app/thumbnails/`, keyed the same way (MD5 of the file
URI, invalidated on mtime and size), generated with QuickLook. The frontend sees
one interface; only the backend differs per platform.

## Follow-up work

Implementation is intentionally out of scope for issue #23. The follow-up should
cover: the Rust cache reader/writer, the thumbnailer-invocation worker, the
result-tile fallback while a thumbnail is pending, and — per `AGENTS.md` — a
keyboard shortcut for toggling thumbnail display with automated coverage.
