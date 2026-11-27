---
name: toka-isolated-run
description: Build a Toka worktree without installing it and run it beside the installed app with its own config, data, cache and indexer. Use when the user wants to test a branch or PR (e.g. "test PR #N before merge", "run this branch", "try the feature") or asks what isolated builds are running.
---

# Isolated Toka run (Linux and macOS)

Builds and runs a worktree's Toka **without touching the installed app**. The
isolated instance never touches the installed app, the user's real index or the
real indexer service.

Never use `npm run build:linux` / `build:mac` for pre-merge testing: they end
by overwriting the installed app.

All commands run from the worktree under test (or pass `--worktree <path>`):

```sh
node scripts/isolated/toka-isolated.mjs <build|launch|stop|status|env> [flags]
```

If the worktree predates these scripts, call them from another worktree that has
them and pass `--worktree`.

## Test a branch or PR

1. Check for stray builds first: `pgrep -a -x 'rustc|cargo'`. One build at a time.
2. `build`: reproduces the platform build minus the install (`-j 4`, `--no-bundle`),
   then restores the tracked schema file that the build rewrites
   (`src-tauri/gen/schemas/linux-schema.json` on Linux,
   `src-tauri/gen/schemas/macOS-schema.json` on macOS). Warm rebuild about
   1 minute, cold about 30. Run it in the background and wait without tight polling.
   Output: `src-tauri/target/release/toka` (and `ts` beside it).
   Afterwards check `pgrep -a -x 'rustc|cargo'`. Terminate only processes that are
   demonstrably orphans of an interrupted build of this worktree
   (check `ps -o ppid,cwd`); another project's concurrent build is not yours to kill.
3. `launch --detach`: starts the app and its own `ts daemon run`, returns once the
   app is up. Without `--detach` it runs in the foreground and Ctrl-C stops both.
4. Tell the user what is running and where: the `status` output (app and indexer
   PIDs, the isolated home `<worktree>/.scratch/isolated-home`, the binary) and
   which folder is indexed. Say that it runs beside their installed Toka.
5. When the user is done: `stop` (add `--clean` to delete the isolated home).
   Stop before removing the worktree.

## What is isolated, and how

- `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` point at
  `<worktree>/.scratch/isolated-home/{config,data,cache}`: separate settings,
  search folders, index databases, status file and logs. `HOME` also points there
  so that macOS path resolution (`~/Library/…`) stays isolated. One home per
  worktree, so several branches can run at once.
- **The indexer is started by hand** with the same env. The launcher tracks both
  PIDs in `run/state.json`, verifies the command line before signalling, and kills
  both process groups on exit, Ctrl-C or `stop`.
- **Service manager shims** live in `<home>/bin`, which comes first on `PATH`:
  - **Linux**: `systemctl` shim logs each call and acts only on the isolated
    indexer (stop / start `toka-search.service`); every other call is a no-op.
    `loginctl` shim reports lingering as already enabled and no-ops everything
    else, so `ts install` never changes host login state.
  - **macOS**: `launchctl` shim logs to `run/launchctl-shim.log` and stands in
    for the agent: `bootstrap`/`kickstart` mark it loaded and start the isolated
    indexer unless it runs, `bootout` stops it, and `print` answers as launchd
    would from that state. The real launchd agent is never reached.
  Never launch the isolated binary by hand without the env (use `env` below).
- `TMPDIR` is also isolated (`<home>/tmp`): Toka's thumbnail cache lives in the
  temp dir and "reset index" deletes it.
- Trade-off: redirecting `XDG_CONFIG_HOME`/`XDG_CACHE_HOME` also hides GTK/macOS
  theme, font and WebKit settings, so the window may look slightly different (and
  start slower) than the installed app. That is expected.
- A shared `CARGO_TARGET_DIR` means every worktree maps to one binary. `launch`
  runs whichever worktree built it last and does not check the source, so build
  the worktree you want to test immediately before launching it. `stop` and
  `status` still tell runs apart by each process's `XDG_CONFIG_HOME`.
- **Fixtures are a throwaway copy.** First launch copies `test/fixtures` (or
  `--seed-from <dir>`, copied in full, so keep it small) to `<home>/fixtures` and
  indexes that, so Toka's tag/rename-on-play never dirties the worktree. `--no-seed`
  starts empty. Adding a real media folder in the UI indexes it in place and playing
  there can rename files; warn the user before they do.

## macOS notes

- The binary is at `src-tauri/target/release/toka` (non-bundled) after `build`.
  The bundled `.app` at `src-tauri/target/release/bundle/macos/Toka.app` is not
  produced by `build` (which passes `--no-bundle`); it is not needed for
  pre-merge testing.
- Because `dirs` on macOS resolves `~/Library/…` from `$HOME`, setting
  `HOME=<worktree>/.scratch/isolated-home` isolates all app data. The XDG
  variables are still set for the ts indexer, which uses them on all platforms.
- `ts install` on macOS sets up a **launchd agent** (not a systemd unit). The
  `launchctl` shim on `PATH` intercepts all those calls so the real agent is
  never touched. `systemctl` and `loginctl` shims are not written on macOS.
- Process detection (is this PID our binary?) uses `/proc/<pid>/cmdline` and
  `/proc/<pid>/environ` on Linux, and `ps -p <pid> -o args=` / `ps ewww -p <pid>`
  on macOS.

## Using the toka CLI against the isolated home

Run it in a subshell so the shim and XDG variables do not linger in your shell:

```sh
( eval "$(node scripts/isolated/toka-isolated.mjs env)"; <binary from status> <cli command> )
```

## Reporting

Always report: worktree and branch, app and indexer PIDs, isolated home path,
that the installed app and real index were untouched, and how to stop it.
`status --all` lists every worktree with a run or a home.
