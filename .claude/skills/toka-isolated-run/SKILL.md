---
name: toka-isolated-run
description: Build a Toka worktree without installing it and run it beside the installed app with its own config, data, cache and indexer. Use when the user wants to test a branch or PR (e.g. "test PR #N before merge", "run this branch", "try the feature") or asks what isolated builds are running.
---

# Isolated Toka run (Linux)

Builds and runs a worktree's Toka **without touching the installed app**. The
isolated instance never touches the installed app (`~/.local/opt/toka`,
`~/.local/bin/toka`, `toka.desktop`), the user's real index
(`~/.config/toka`, `~/.local/share/toka`) or the real `toka-indexer.service`.

Never use `npm run build:linux` for pre-merge testing: it ends by overwriting the
installed app.

All commands run from the worktree under test (or pass `--worktree <path>`):

```sh
node scripts/isolated/toka-isolated.mjs <build|launch|stop|status|env> [flags]
```

If the worktree predates these scripts, call them from another worktree that has
them and pass `--worktree`.

## Test a branch or PR

1. Check for stray builds first: `pgrep -a -f 'rustc|cargo'`. One build at a time.
2. `build`: reproduces `bin/build.mjs` minus the install (`-j 4`, `--no-bundle`),
   then restores the tracked `src-tauri/gen/schemas/linux-schema.json` that the
   build rewrites. Warm rebuild about 1 minute, cold about 30. Run it in the
   background and wait without tight polling.
   Output: `src-tauri/target/release/toka`. Afterwards check
   `pgrep -a -f 'rustc|cargo'` and kill leftovers.
3. `launch --detach`: starts the app and its own `--indexer`, returns once the
   app is up. Without `--detach` it runs in the foreground and Ctrl-C stops both.
4. Tell the user what is running and where: the `status` output (app and indexer
   PIDs, the isolated home `<worktree>/.scratch/isolated-home`, the binary) and
   which folder is indexed. Say that it runs beside their installed Toka.
5. When the user is done: `stop` (add `--clean` to delete the isolated home).
   Stop before removing the worktree.

## What is isolated, and how

- `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` point at
  `<worktree>/.scratch/isolated-home/{config,data,cache}`: separate settings,
  search folders, index databases, status file and logs. One home per worktree,
  so several branches can run at once.
- **The indexer is started by hand** with the same env. The app's own
  `systemctl --user start toka-indexer.service` would otherwise be a no-op against
  the user's system indexer, which reads the real config, so isolated folders
  would never be indexed. The launcher tracks both PIDs in
  `run/state.json`, verifies the command line before signalling, and kills both
  process groups on exit, Ctrl-C or `stop`.
- **`systemctl` is shimmed.** `<home>/bin` comes first on PATH with a `systemctl`
  that logs to `run/systemctl-shim.log` and succeeds. This is what stops the
  isolated app's "reset index" from running `systemctl --user stop
  toka-indexer.service` against the user's real indexer. Never launch the
  isolated binary by hand without the env (use `env` below).
- **Fixtures are a throwaway copy.** First launch copies `test/fixtures` (or
  `--seed-from <dir>`) to `<home>/fixtures` and indexes that, so Toka's
  tag/rename-on-play never dirties the worktree. `--no-seed` starts empty. Adding
  a real media folder in the UI indexes it in place and playing there can rename
  files; warn the user before they do.

## Using the toka CLI against the isolated home

`eval "$(node scripts/isolated/toka-isolated.mjs env)"` exports the same
variables in the current shell (including the shim), so
`src-tauri/target/release/toka <cli command>` acts on the isolated home only.

## Reporting

Always report: worktree and branch, app and indexer PIDs, isolated home path,
that the installed app and real index were untouched, and how to stop it.
`status --all` lists every worktree with a run or a home.

## macOS (not implemented)

No systemd and no XDG: Toka's data lives under `~/Library/Application Support/app.toka.desktop`
and caches under `~/Library/Caches`, which the `dirs` crate derives from `$HOME`.
Launching the built `.app` binary with `HOME=<worktree>/.scratch/isolated-home`
should isolate it, but is untested; check how the macOS indexer is started before
relying on it. The script refuses to run on non-Linux.
