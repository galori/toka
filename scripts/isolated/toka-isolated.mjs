#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
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
  currentIndexerPid,
  isIsolatedIndexerProcess,
  isIsolatedProcess,
  isSupervisorOf,
  isolatedEnv,
  isolatedPaths,
  launchctlShimScript,
  loginctlShimScript,
  parseArguments,
  readState,
  seedConfig,
  shimScript,
  tsBuildInvocation,
} from "./lib.mjs";

const scriptPath = fileURLToPath(import.meta.url);

const USAGE = `Usage: node scripts/isolated/toka-isolated.mjs <command> [flags]

Runs a worktree's build of Toka with its own config, data, cache and indexer,
beside the installed app. It never installs anything and never touches the
installed app, the user's data directories or the indexer service.

Commands:
  build    Build the worktree's release binaries without installing them.
  launch   Start the app and its own indexer (--detach to return immediately).
  stop     Stop the app and indexer (--clean also deletes the isolated home).
  status   Show what is running, per worktree (--all lists every worktree).
  env      Print shell exports that point a shell at the isolated home, so
           'eval "$(... env)"' lets the toka CLI act on it.

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

// ---------------------------------------------------------------- build

async function build(flags) {
  const worktree = targetWorktree(flags);
  const paths = isolatedPaths(worktree);
  // The build rewrites this tracked file. Restore it afterwards, but only if it
  // was clean going in, so a deliberate local edit is never discarded.
  const schemaWasClean =
    spawnSync("git", ["diff", "--quiet", "--", paths.schemaFile], {
      cwd: worktree,
    }).status === 0;
  const schemaBackup = schemaWasClean
    ? null
    : readFileSync(path.join(worktree, paths.schemaFile));
  const busy = spawnSync("pgrep", ["-x", "rustc|cargo"], { encoding: "utf8" });
  if (busy.stdout.trim()) {
    console.error(
      "Warning: cargo/rustc processes are already running; builds compete for the CPU.",
    );
  }
  const load = (file) => import(pathToFileURL(path.join(worktree, file)).href);
  const { prepareBuild } = await load("bin/prepare-build.mjs");
  const info = prepareBuild();
  // `ts` has no build script, so it ignores VITE_GIT_SHA: without its own
  // provenance `ts --version` shows `(Git SHA development)` next to an app
  // showing the real SHA.
  const tsGitSha = /^[0-9a-f]+$/i.test(info.gitSha ?? "")
    ? info.gitSha
    : undefined;
  const env = {
    ...process.env,
    CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "4",
    VITE_APP_VERSION: info.buildVersion,
    VITE_BUILD_TIME: info.builtAt,
    VITE_GIT_SHA: info.gitSha,
    TS_BUILD_VERSION: info.buildVersion,
    ...(tsGitSha === undefined ? {} : { TOKA_GIT_SHA: tsGitSha }),
  };
  const child = spawn(
    "npm",
    ["run", "tauri", "build", "--", "--no-bundle"],
    { cwd: worktree, env, stdio: "inherit", detached: true },
  );
  child.on("error", (error) => {
    console.error(`Could not run npm: ${error.message}`);
    process.exit(1);
  });
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
  if (schemaWasClean) {
    spawnSync("git", ["checkout", "--", paths.schemaFile], { cwd: worktree });
  } else {
    // The build rewrote a file that had local edits; put them back.
    writeFileSync(path.join(worktree, paths.schemaFile), schemaBackup);
  }
  // The search helper beside the app: the isolated app resolves it as its
  // sibling, and the isolated indexer runs from it.
  if (status === 0) {
    const tsBuild = tsBuildInvocation(paths);
    const ts = spawnSync(tsBuild.command, tsBuild.args, {
      cwd: tsBuild.cwd,
      env,
      stdio: "inherit",
    });
    if ((ts.status ?? 1) !== 0) process.exit(ts.status ?? 1);
  }
  // Leftovers cannot be attributed to this build reliably, so report them and
  // leave killing them to the caller (AGENTS.md: check, then terminate).
  const leftovers = spawnSync("pgrep", ["-a", "-x", "rustc|cargo"], {
    encoding: "utf8",
  });
  if (leftovers.stdout.trim()) {
    console.error(
      `cargo/rustc processes still running after the build; terminate them if they are orphans:\n${leftovers.stdout}`,
    );
  }
  if (status !== 0) process.exit(status);
  console.log(`Built (not installed): ${paths.binary} and ${paths.tsBinary}`);
}

// --------------------------------------------------------------- launch

function prepareHome(paths, flags) {
  for (const directory of [
    paths.config,
    paths.data,
    paths.cache,
    paths.shimDir,
    paths.runDir,
    paths.tmp,
  ]) {
    mkdirSync(directory, { recursive: true });
  }
  if (process.platform === "linux") {
    writeFileSync(paths.shim, shimScript(paths));
    chmodSync(paths.shim, 0o755);
    writeFileSync(paths.loginctlShim, loginctlShimScript());
    chmodSync(paths.loginctlShim, 0o755);
  } else if (process.platform === "darwin") {
    writeFileSync(paths.launchctlShim, launchctlShimScript(paths));
    chmodSync(paths.launchctlShim, 0o755);
  }
  if (flags["no-seed"] || existsSync(paths.indexFolders)) return;
  const source = path.resolve(
    paths.worktree,
    flags["seed-from"] ?? "test/fixtures",
  );
  if (paths.home === source || paths.home.startsWith(`${source}${path.sep}`)) {
    fail(
      `--seed-from ${source} contains the isolated home; pick a smaller folder.`,
    );
  }
  if (!existsSync(source)) {
    console.error(`Seed folder ${source} not found; starting with no folders.`);
    return;
  }
  // A throwaway copy: Toka renames and tags what it plays, which would dirty
  // the tracked fixtures.
  if (!existsSync(paths.fixtures)) {
    // Copy beside the target and rename, so an interrupted copy is never taken
    // for a finished one.
    const partial = `${paths.fixtures}.partial`;
    rmSync(partial, { recursive: true, force: true });
    // Dereference, so a symlinked source is copied rather than linked back to
    // the original media.
    cpSync(source, partial, { recursive: true, dereference: true });
    renameSync(partial, paths.fixtures);
  }
  mkdirSync(path.dirname(paths.indexFolders), { recursive: true });
  writeFileSync(
    paths.indexFolders,
    `${JSON.stringify(seedConfig([paths.fixtures], randomUUID), null, 2)}\n`,
  );
}

function groupAlive(pid) {
  try {
    process.kill(-pid, 0);
    return true;
  } catch {
    return false;
  }
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
  const worktree = targetWorktree(flags);
  const paths = isolatedPaths(worktree);
  if (!existsSync(paths.binary) || !existsSync(paths.tsBinary)) {
    fail(
      `No built binaries at ${paths.binary} and ${paths.tsBinary}. Run: node scripts/isolated/toka-isolated.mjs build`,
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
      if (!isSupervisorOf(supervisor.pid, scriptPath)) break;
      await sleepMs(100);
    }
    killGroup(supervisor.pid, "SIGTERM");
    fail(
      `The app did not start (it exited or took over 15s). See ${paths.supervisorLog} and ${path.join(paths.runDir, "app.log")}`,
    );
  }

  const env = isolatedEnv(paths);
  delete env.TOKA_ISOLATED_SUPERVISOR;
  const stdio = (name) =>
    supervising
      ? ["ignore", openSync(path.join(paths.runDir, name), "a"), "inherit"]
      : "inherit";
  const indexer = spawn(paths.tsBinary, indexerArguments(), {
    cwd: worktree,
    env,
    detached: true,
    stdio: ["ignore", openSync(paths.indexerLog, "a"), "inherit"],
  });
  indexer.on("error", (error) => {
    console.error(`Could not start the indexer: ${error.message}`);
    process.exit(1);
  });
  writeFileSync(paths.indexerPidFile, `${indexer.pid}\n`);
  const app = spawn(paths.binary, [], {
    cwd: worktree,
    env,
    detached: true,
    stdio: stdio("app.log"),
  });
  app.on("error", (error) => {
    console.error(`Could not start the app: ${error.message}`);
    killGroup(indexer.pid, "SIGKILL");
    rmSync(paths.state, { force: true });
    process.exit(1);
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

  let cleaning = null;
  // SIGTERM both groups, wait for them to go, SIGKILL whatever survives, and
  // only then drop the state file, so a survivor is never left untracked.
  const cleanup = () => {
    cleaning ??= (async () => {
      const liveIndexer = currentIndexerPid(paths, null);
      const groups = [
        app.pid,
        [liveIndexer, indexer.pid].find(
          (pid) => pid !== null && isIsolatedIndexerProcess(pid, paths),
        ),
      ].filter((pid) => pid !== undefined);
      for (const pid of groups) killGroup(pid, "SIGTERM");
      for (
        let waited = 0;
        waited < 3000 && groups.some(groupAlive);
        waited += 100
      ) {
        await sleepMs(100);
      }
      for (const pid of groups) if (groupAlive(pid)) killGroup(pid, "SIGKILL");
      rmSync(paths.state, { force: true });
    })();
    return cleaning;
  };
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(signal, () => cleanup().then(() => process.exit(0)));
  }
  app.on("exit", (code) => cleanup().then(() => process.exit(code ?? 0)));
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
    const appPid = state.appPid;
    const indexerPid = currentIndexerPid(paths, state);
    if (isIsolatedProcess(appPid, paths)) killGroup(appPid, "SIGTERM");
    if (isIsolatedIndexerProcess(indexerPid, paths))
      killGroup(indexerPid, "SIGTERM");
    for (let waited = 0; waited < 5000; waited += 100) {
      if (!describeRun(paths, state).running) break;
      await sleepMs(100);
    }
    if (isIsolatedProcess(appPid, paths)) killGroup(appPid, "SIGKILL");
    if (isIsolatedIndexerProcess(indexerPid, paths))
      killGroup(indexerPid, "SIGKILL");
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
  const here = targetWorktree(flags);
  const candidates = flags.all || !flags.worktree ? gitWorktrees(here) : [here];
  const runs = candidates
    .map((worktree) => describeRun(isolatedPaths(worktree)))
    .filter(
      (run) => run.running || existsSync(run.home) || run.worktree === here,
    );
  const interesting = flags.all
    ? runs
    : runs.filter((run) => run.running || run.worktree === here);
  if (flags.json) {
    console.log(JSON.stringify(interesting, null, 2));
    return;
  }
  if (interesting.length === 0) console.log("No isolated runs.");
  for (const run of interesting) report(run);
}

// ------------------------------------------------------------------ env

function printEnv(flags) {
  const paths = isolatedPaths(targetWorktree(flags));
  mkdirSync(paths.shimDir, { recursive: true });
  mkdirSync(paths.runDir, { recursive: true });
  if (process.platform === "linux") {
    writeFileSync(paths.shim, shimScript(paths));
    chmodSync(paths.shim, 0o755);
    writeFileSync(paths.loginctlShim, loginctlShimScript());
    chmodSync(paths.loginctlShim, 0o755);
  } else if (process.platform === "darwin") {
    writeFileSync(paths.launchctlShim, launchctlShimScript(paths));
    chmodSync(paths.launchctlShim, 0o755);
  }
  const env = isolatedEnv(paths, {});
  mkdirSync(paths.tmp, { recursive: true });
  const quoted = (value) => `'${value.replaceAll("'", "'\\''")}'`;
  // HOME is exported too: without it a shell eval'ing this would still
  // resolve the real home, and the toka CLI would act on it.
  for (const key of [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "TMPDIR",
  ]) {
    console.log(`export ${key}=${quoted(env[key])}`);
  }
  console.log(`export PATH=${quoted(paths.shimDir)}:"$PATH"`);
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
