# Optimizing `npm run build:linux`

Measurements taken 2026-07-27 on the Linux development machine (4 cores, 16 GB
RAM), with a warm Cargo cache, against commit `b55f085`.

## Where the time went

A warm `npm run build:linux` took about **2 minutes 30 seconds**:

| Phase                                | Time    | Notes                                        |
| ------------------------------------ | ------- | -------------------------------------------- |
| `tsc` + `vite build`                 | ~5s     | negligible                                    |
| `cargo build --release` (app crate)  | ~55–60s | ran on **every** build, even with no changes  |
| AppImage bundling                    | ~86s    | linuxdeploy + squashfs of an 80 MB image      |
| Install into `~/.local`              | <1s     |                                               |

Only the app crate ever recompiled; the dependency graph stayed cached. So the
build was roughly 58% packaging, 38% one crate, 4% everything else.

## 1. AppImage bundling bought nothing locally — fixed

`bin/build.mjs` passed `--bundles appimage`, and `scripts/install-linux.mjs`
copied the resulting 80 MB image into `~/.local/opt/toka`. An AppImage exists to
carry GTK and WebKit to machines that lack them, but the machine doing the build
is the one that just compiled against those libraries.

`src-tauri/target/release/toka` (9.6 MB) is already built and runnable before
bundling starts. `build:linux` now builds with `--no-bundle` and installs that
executable directly, with the desktop entry's `Exec=` pointing at it.

Pass `--appimage` (`npm run build:linux -- --appimage`) to produce and install a
real AppImage when you want to check the packaged artifact. CI is unaffected:
`.github/workflows/ci.yml` runs its own `tauri build --bundles`, and never calls
`bin/build.mjs`.

**Saved: ~86s per build.**

## 2. The Rust recompile fired unconditionally — fixed

Two experiments isolated this:

- `cargo build --release` twice in a row, nothing touched, finished the second
  time in **0.24s**. Cargo is perfectly capable of a no-op here.
- A full build with **byte-identical** `dist/` output still recompiled the crate.

The cause is that `vite build` rewrites every file in `dist/` on each run, which
bumps their timestamps. `tauri-build` watches the frontend directory, and
because `generate_context!` embeds the frontend into the binary, the whole crate
rebuilds. A Rust-only change, or no change at all, still cost a full optimized
rebuild.

Two changes were needed, because either alone leaves the invalidation in place:

**Stable timestamps.** `bin/build-frontend.mjs` now builds into a staging
directory under `node_modules/.cache/`, then `bin/sync-dist.mjs` copies into
`dist/` only the files whose contents actually differ, removes ones the build no
longer emits, and leaves everything else untouched. This sits inside
`npm run build`, so every caller benefits — `check:pr`, `build:integration`, and
Tauri's `beforeBuildCommand` alike.

**Stable contents.** `bin/prepare-build.mjs` injected `VITE_BUILD_TIME` as a
fresh `new Date()`, and `src/buildInfo.ts` inlines it into the JS bundle. That
alone guaranteed different output on every run. It now dates a clean tree by its
HEAD commit time, falling back to wall clock when the tree is dirty, since
uncommitted work has no commit time.

The app's "Built:" line therefore reports when the code was committed rather
than when it was packaged. That is the better staleness signal — it is what the
adjacent "commits behind origin/main" readout already measures against — and it
makes rebuilding an unchanged commit reproducible. `bin/toka-freshness.mjs`
reports the same value and needed no change.

**Saved: ~55s on any build whose frontend output is unchanged.**

## Result

| Scenario                     | Before | After |
| ---------------------------- | ------ | ----- |
| Nothing changed (clean tree) | ~150s  | 4.2s  |
| Rust or frontend change      | ~150s  | ~59s  |

A frontend change still pays the crate rebuild — unavoidable while the assets
are embedded in the binary.

Two caveats on the no-op case. It needs a committed tree: with uncommitted
changes the timestamp falls back to wall clock by design, and the crate rebuilds
as before. And it needs consecutive builds to go through the same entry point —
alternating `npm run build:linux` with a bare `cargo build` invalidates the
crate each way round, because the Tauri CLI sets `TAURI_CONFIG`, which the
generated build script declares via `cargo:rerun-if-env-changed`.

## 3. Not done: one target directory per worktree

`AGENTS.md` puts every task in its own worktree, and each gets a private
`src-tauri/target/`. At the time of measurement: 17 GB in the repository root,
29 GB in one worktree, roughly 10 GB across the rest. Beyond the disk cost,
every new worktree pays a full cold dependency build.

A shared `CARGO_TARGET_DIR` fixes both, but it serializes concurrent builds on
Cargo's lock, which contradicts the "Keep builds parallel" intent in
`bin/build.mjs`. On 4 cores that is arguably the right trade anyway — parallel
builds are not buying throughput, they are just oversubscribing the CPU. A
background `cargo test` in another worktree was observed inflating a foreground
build from 57s to 1m47s.

`scripts/install-linux.mjs` already honours `CARGO_TARGET_DIR`, so the switch is
just setting the variable.

The alternative is `sccache`, which shares compiled dependencies across
worktrees without lock contention, but it does not cache the final link and
would not help the app crate — which is the only thing that recompiles here.

## 4. Not done: release profile and linker

`src-tauri/Cargo.toml` has no `[profile.release]` section and there is no
`.cargo/config.toml`, so the build runs on stock settings: `opt-level = 3`,
`codegen-units = 16`, `incremental = false`.

- `incremental = true` for the release profile would cut repeat edits to the app
  crate, at the cost of some runtime performance and disk.
- Linking uses stock GNU `ld`; `mold`, `lld`, and `clang` are all absent from
  the machine. A faster linker is the usual next win for a large binary like
  this one.

The link portion of those ~55s was never isolated, so neither change is sized.
Run `cargo build --release --timings` and read the codegen/link split before
deciding whether either is worth it.
