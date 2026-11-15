#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  openSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { pathToFileURL, fileURLToPath } from "node:url";
import {
  describeRun,
  gitToplevel,
  gitWorktrees,
  indexerArguments,
  isProcessOf,
  isSupervisorOf,
  isolatedEnv,
  isolatedPaths,
  parseArguments,
  readState,
  seedConfig,
  shimScript,
} from "./lib.mjs";

const scriptPath = fileURLToPath(import.meta.url);

const USAGE = `Usage: node scripts/isolated/toka-isolated.mjs <command> [flags]

Runs a worktree's build of Toka with its own config, data, cache and indexer,
beside the installed app. It never installs anything and never touches the
installed app, ~/.config/toka, ~/.local/share/toka or toka-indexer.service.

Commands:
  build    Build the worktree's release binary without installing it.
  launch   Start the app and its own indexer (--detach to return immediately).
  stop     Stop the app and indexer (--clean also deletes the isolated home).
  status   Show what is running, per worktree (--all lists every worktree).
  env      Print shell exports that point a shell at the isolated home, so
           "eval \\"$(... env)\\"" lets the toka CLI act on it.

Flags:
  --worktree <path>   Target this worktree (default: the current one).
  --seed-from <dir>   Folder copied into the isolated home and indexed on
                      first launch (default: test/fixtures).
  --no-seed           Start with no search folders.
  --detach            launch: run in the background; stop it with "stop".
  --clean             stop: also delete .scratch/isolated-home.
  --all               status: list every git worktree.
  --json              status: machine-readable output.
`;

function fail(message) {
  console.error(message);
  process.exit(1);
}

function sleepMs(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function targetWorktree(flags) {
  return gitToplevel(path.resolve(flags.worktree ?? process.cwd()));
}

function requireLinux() {
  if (process.platform !== "linux") {
    fail(
      "Isolated runs are Linux-only for now (see the toka-isolated-run skill for the macOS notes).",
    );
  }
}

// ---------------------------------------------------------------- build

async function build(flags) {
  requireLinux();
  const worktree = targetWorktree(flags);
  const paths = isolatedPaths(worktree);
  const busy = spawnSync("pgrep", ["-f", "rustc|cargo"], { encoding: "utf8" });
  if (busy.stdout.trim()) {
    console.error(
      "Warning: cargo/rustc processes are already running; builds compete for the CPU.",
    );
  }
  const load = (file) => import(pathToFileURL(path.join(worktree, file)).href);
  const { buildPlocate } = await load("scripts/build-plocate.mjs");
  const { prepareBuild } = await load("bin/prepare-build.mjs");
  await buildPlocate();
  const info = prepareBuild();
  const env = {
    ...process.env,
    CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "4",
    VITE_APP_VERSION: info.version,
    VITE_BUILD_TIME: info.builtAt,
    VITE_GIT_SHA: info.gitSha,
  };
  const child = spawn(
    "npm",
    [
      "run",
      "tauri",
      "build",
      "--",
      "--no-bundle",
      "--config",
      "src-tauri/tauri.linux-owned-index.conf.json",
    ],
    { cwd: worktree, env, stdio: "inherit", detached: true },
  );
  const forward = (signal) => () => {
    try {
      process.kill(-child.pid, signal);
    } catch {
      /* already gone */
    }
  };
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(signal, forward(signal));
  }
  const status = await new Promise((resolve) =>
    child.on("exit", (code, signal) => resolve(code ?? (signal ? 1 : 0))),
  );
  // The build rewrites this tracked file; never let it reach a commit.
  spawnSync("git", ["checkout", "--", paths.schemaFile], { cwd: worktree });
  const leftovers = spawnSync(
    "pgrep",
    ["-f", "-a", `${worktree}.*(rustc|cargo)`],
    {
      encoding: "utf8",
    },
  );
  if (leftovers.stdout.trim()) {
    console.error(`Leftover build processes:\n${leftovers.stdout}`);
  }
  if (status !== 0) process.exit(status);
  console.log(`Built (not installed): ${paths.binary}`);
}

// --------------------------------------------------------------- launch

function prepareHome(paths, flags) {
  for (const directory of [
    paths.config,
    paths.data,
    paths.cache,
    paths.shimDir,
    paths.runDir,
  ]) {
    mkdirSync(directory, { recursive: true });
  }
  writeFileSync(paths.shim, shimScript(paths.shimLog));
  chmodSync(paths.shim, 0o755);
  if (flags["no-seed"] || existsSync(paths.indexFolders)) return;
  const source = path.resolve(
    paths.worktree,
    flags["seed-from"] ?? "test/fixtures",
  );
  if (!existsSync(source)) {
    console.error(`Seed folder ${source} not found; starting with no folders.`);
    return;
  }
  // A throwaway copy: Toka renames and tags what it plays, which would dirty
  // the tracked fixtures.
  if (!existsSync(paths.fixtures)) {
    cpSync(source, paths.fixtures, { recursive: true, mode: 1 });
  }
  mkdirSync(path.dirname(paths.indexFolders), { recursive: true });
  writeFileSync(
    paths.indexFolders,
    `${JSON.stringify(seedConfig([paths.fixtures], randomUUID), null, 2)}\n`,
  );
}

function killGroup(pid, signal) {
  try {
    process.kill(-pid, signal);
  } catch {
    try {
      process.kill(pid, signal);
    } catch {
      /* already gone */
    }
  }
}

async function launch(flags) {
  requireLinux();
  const worktree = targetWorktree(flags);
  const paths = isolatedPaths(worktree);
  if (!existsSync(paths.binary)) {
    fail(
      `No built binary at ${paths.binary}. Run: node scripts/isolated/toka-isolated.mjs build`,
    );
  }
  const supervising = process.env.TOKA_ISOLATED_SUPERVISOR === "1";
  const existing = describeRun(paths);
  if (existing.running && !supervising) {
    fail(
      `Already running for ${paths.name} (app ${existing.appPid}, indexer ${existing.indexerPid}). Stop it first.`,
    );
  }
  prepareHome(paths, flags);

  if (flags.detach && !supervising) {
    const log = openSync(paths.supervisorLog, "a");
    const args = [scriptPath, "launch", "--worktree", worktree];
    if (flags["no-seed"]) args.push("--no-seed");
    if (flags["seed-from"]) args.push("--seed-from", flags["seed-from"]);
    const supervisor = spawn(process.execPath, args, {
      detached: true,
      stdio: ["ignore", log, log],
      env: { ...process.env, TOKA_ISOLATED_SUPERVISOR: "1" },
    });
    supervisor.unref();
    for (let waited = 0; waited < 15000; waited += 100) {
      if (describeRun(paths).appRunning) {
        report(describeRun(paths));
        return;
      }
      await sleepMs(100);
    }
    fail(`The app did not start; see ${paths.supervisorLog}`);
  }

  const env = isolatedEnv(paths);
  const stdio = (name) =>
    supervising
      ? ["ignore", openSync(path.join(paths.runDir, name), "a"), "inherit"]
      : "inherit";
  const indexer = spawn(paths.binary, indexerArguments(), {
    cwd: worktree,
    env,
    detached: true,
    stdio: [
      "ignore",
      openSync(path.join(paths.runDir, "indexer.log"), "a"),
      "inherit",
    ],
  });
  const app = spawn(paths.binary, [], {
    cwd: worktree,
    env,
    detached: true,
    stdio: stdio("app.log"),
  });
  writeFileSync(
    paths.state,
    `${JSON.stringify({
      worktree,
      supervisorPid: process.pid,
      appPid: app.pid,
      indexerPid: indexer.pid,
      startedAt: new Date().toISOString(),
    })}\n`,
  );
  if (!supervising) report(describeRun(paths));

  let cleaned = false;
  const cleanup = () => {
    if (cleaned) return;
    cleaned = true;
    killGroup(app.pid, "SIGTERM");
    killGroup(indexer.pid, "SIGTERM");
    setTimeout(() => {
      killGroup(app.pid, "SIGKILL");
      killGroup(indexer.pid, "SIGKILL");
    }, 3000).unref();
    rmSync(paths.state, { force: true });
  };
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(signal, () => {
      cleanup();
      setTimeout(() => process.exit(0), 500);
    });
  }
  indexer.on("exit", () => {
    // An indexer that died on its own is worth knowing about, but the app
    // keeps running; only app exit ends the session.
  });
  app.on("exit", (code) => {
    cleanup();
    setTimeout(() => process.exit(code ?? 0), 100);
  });
}

// ----------------------------------------------------------------- stop

async function stopOne(paths, { clean }) {
  const state = readState(paths);
  if (state) {
    if (isSupervisorOf(state.supervisorPid, scriptPath)) {
      try {
        process.kill(state.supervisorPid, "SIGTERM");
      } catch {
        /* gone */
      }
    }
    for (const pid of [state.appPid, state.indexerPid]) {
      if (isProcessOf(pid, paths.binary)) killGroup(pid, "SIGTERM");
    }
    for (let waited = 0; waited < 5000; waited += 100) {
      if (!describeRun(paths, state).running) break;
      await sleepMs(100);
    }
    for (const pid of [state.appPid, state.indexerPid]) {
      if (isProcessOf(pid, paths.binary)) killGroup(pid, "SIGKILL");
    }
    await sleepMs(100);
    if (describeRun(paths, state).running) {
      console.error(`Could not stop ${paths.name}.`);
      return false;
    }
    rmSync(paths.state, { force: true });
    console.log(`Stopped ${paths.name}.`);
  } else {
    console.log(`Nothing running for ${paths.name}.`);
  }
  if (clean) {
    if (!paths.home.endsWith(path.join(".scratch", "isolated-home"))) {
      fail(`Refusing to delete ${paths.home}.`);
    }
    rmSync(paths.home, { recursive: true, force: true });
    console.log(`Deleted ${paths.home}`);
  }
  return true;
}

// --------------------------------------------------------------- status

function report(run) {
  console.log(
    `${run.name}: app ${run.appRunning ? `pid ${run.appPid}` : "stopped"}, indexer ${run.indexerRunning ? `pid ${run.indexerPid}` : "stopped"}\n  home: ${run.home}\n  binary: ${run.binary}${run.built ? "" : " (not built)"}`,
  );
}

function status(flags) {
  requireLinux();
  const here = targetWorktree(flags);
  const candidates = flags.all || !flags.worktree ? gitWorktrees(here) : [here];
  const runs = candidates
    .map((worktree) => describeRun(isolatedPaths(worktree)))
    .filter(
      (run) => run.running || existsSync(run.home) || run.worktree === here,
    );
  if (flags.json) {
    console.log(JSON.stringify(runs, null, 2));
    return;
  }
  const interesting = flags.all
    ? runs
    : runs.filter((run) => run.running || run.worktree === here);
  if (interesting.length === 0) console.log("No isolated runs.");
  for (const run of interesting) report(run);
}

// ------------------------------------------------------------------ env

function printEnv(flags) {
  const paths = isolatedPaths(targetWorktree(flags));
  mkdirSync(paths.shimDir, { recursive: true });
  mkdirSync(paths.runDir, { recursive: true });
  writeFileSync(paths.shim, shimScript(paths.shimLog));
  chmodSync(paths.shim, 0o755);
  const env = isolatedEnv(paths, { PATH: "$PATH" });
  for (const key of ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"]) {
    console.log(`export ${key}='${env[key]}'`);
  }
  console.log(`export PATH='${paths.shimDir}':"$PATH"`);
}

// ----------------------------------------------------------------- main

let parsed;
try {
  parsed = parseArguments(process.argv.slice(2));
} catch (error) {
  console.error(error.message);
  console.error(USAGE);
  process.exit(2);
}
const { command, flags } = parsed;
if (command === "help" || flags.help) {
  console.log(USAGE);
} else if (command === "build") {
  await build(flags);
} else if (command === "launch") {
  await launch(flags);
} else if (command === "stop") {
  requireLinux();
  const worktree = targetWorktree(flags);
  const targets = flags.all ? gitWorktrees(worktree) : [worktree];
  let ok = true;
  for (const target of targets) {
    ok = (await stopOne(isolatedPaths(target), { clean: flags.clean })) && ok;
  }
  process.exit(ok ? 0 : 1);
} else if (command === "status") {
  status(flags);
} else if (command === "env") {
  printEnv(flags);
}
