# TODO

Future work that is deliberately not tracked as a GitHub issue, because it is
not wanted yet. Anything here is potential tech debt to revisit only if it
starts to matter.

## Make a tagged file findable again after Toka restarts

Toka finds videos through a system index — `plocate` on Linux, Spotlight on
macOS — that is rebuilt on a schedule rather than when a file changes. Tagging
renames the file, so until that rebuild the index still holds the old name, and
`plocate --existing` drops the entry entirely because nothing is behind it any
more. Either way the file stops turning up in a search.

Toka now remembers the files it renamed for as long as it is running and folds
them back into every search, so tagging a video no longer loses it. That memory
does not survive a restart: quit Toka after tagging a video, and the video stays
unfindable until the index is rebuilt on its own schedule (daily, on this
machine).

Options, roughly in order of how general they are:

- Persist the renames Toka has made and reload them at startup, discarding an
  entry once the index has caught up with it. Fully general, but it is a cache
  that can drift from the filesystem and has to be pruned somehow.
- Ask the platform index to re-scan the renamed file. There is no unprivileged
  way to do this with `plocate`: its media index is rebuilt by
  `/usr/local/sbin/plocate-media-updatedb` as root (see
  `~/workspace/ubuntu-bootstrap/README.md`), which would mean shipping a
  sudoers rule or a helper service. `mdimport` can do it unprivileged on macOS,
  so the two platforms would not share an implementation.
- Index media files in Toka itself and stop depending on a system index. The
  most control and the most work.

The first option is the one to reach for if this becomes a real problem.
