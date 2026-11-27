import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readlinkSync } from "node:fs";
import path from "node:path";

/**
 * Pure helpers for running one worktree's build of Toka in isolation from the
 * installed app. Everything here is deterministic path/env construction or
 * argument parsing, so it can be tested without building or launching.
 */

export const COMMANDS = ["build", "launch", "stop", "status", "env"];

const BOOLEAN_FLAGS = new Set([
  "detach",
  "clean",
  "json",
  "no-seed",
  "all",
  "help",
]);
const VALUE_FLAGS = new Set(["worktree", "seed-from"]);

/** Parses `<command> [flags]`; throws on anything unknown. */
export function parseArguments(argv) {
  const [command, ...rest] = argv;
  if (!command || command === "--help" || command === "-h") {
    return { command: "help", flags: {} };
  }
  if (!COMMANDS.includes(command)) {
    throw new Error(
      `Unknown command "${command}". Expected one of: ${COMMANDS.join(", ")}.`,
    );
  }
  const flags = {};
  for (let index = 0; index < rest.length; index += 1) {
    const argument = rest[index];
    if (argument === "--help" || argument === "-h") {
      flags.help = true;
      continue;
    }
    if (!argument.startsWith("--")) {
      throw new Error(`Unexpected argument "${argument}".`);
    }
    const [name, inline] = argument.slice(2).split(/=(.*)/s, 2);
    if (BOOLEAN_FLAGS.has(name)) {
      if (inline !== undefined) throw new Error(`--${name} takes no value.`);
      flags[name] = true;
    } else if (VALUE_FLAGS.has(name)) {
      const value = inline ?? rest[(index += 1)];
      if (!value || value.startsWith("--"))
        throw new Error(`--${name} needs a value.`);
      flags[name] = value;
    } else {
      throw new Error(`Unknown flag "--${name}".`);
    }
  }
  return { command, flags };
}

/** Every location an isolated run uses, all under `<worktree>/.scratch`. */
export function isolatedPaths(worktree, env = process.env) {
  const home = path.join(worktree, ".scratch", "isolated-home");
  const targetDir = env.CARGO_TARGET_DIR
    ? path.resolve(worktree, "src-tauri", env.CARGO_TARGET_DIR)
    : path.join(worktree, "src-tauri", "target");
  // The schema file the tauri build rewrites varies by platform.
  const schemaFile =
    process.platform === "darwin"
      ? "src-tauri/gen/schemas/macOS-schema.json"
      : "src-tauri/gen/schemas/linux-schema.json";
  return {
    worktree,
    name: path.basename(worktree),
    home,
    config: path.join(home, "config"),
    data: path.join(home, "data"),
    cache: path.join(home, "cache"),
    shimDir: path.join(home, "bin"),
    // Linux shims: systemctl and loginctl
    shim: path.join(home, "bin", "systemctl"),
    loginctlShim: path.join(home, "bin", "loginctl"),
    shimLog: path.join(home, "run", "systemctl-shim.log"),
    // macOS shim: launchctl
    launchctlShim: path.join(home, "bin", "launchctl"),
    launchctlShimLog: path.join(home, "run", "launchctl-shim.log"),
    indexerPidFile: path.join(home, "run", "indexer.pid"),
    indexerLog: path.join(home, "run", "indexer.log"),
    tmp: path.join(home, "tmp"),
    fixtures: path.join(home, "fixtures"),
    runDir: path.join(home, "run"),
    state: path.join(home, "run", "state.json"),
    supervisorLog: path.join(home, "run", "supervisor.log"),
    indexFolders: path.join(home, "config", "toka", "index-folders.json"),
    binary: path.join(targetDir, "release", "toka"),
    tsBinary: path.join(targetDir, "release", "ts"),
    schemaFile,
  };
}

/**
 * The environment an isolated Toka and its indexer run with. The XDG variables
 * give them their own settings, index and logs. HOME points at the isolated
 * home too: `ts install` resolves the legacy service file from it, so the
 * real home stays untouched. The PATH shims keep either
 * process from reaching the user's service through systemctl/launchctl
 * or changing host login state through loginctl.
 */
export function isolatedEnv(paths, base = process.env) {
  return {
    ...base,
    HOME: paths.home,
    XDG_CONFIG_HOME: paths.config,
    XDG_DATA_HOME: paths.data,
    XDG_CACHE_HOME: paths.cache,
    // Toka keeps its thumbnail cache under the temp dir, and reset deletes it.
    TMPDIR: paths.tmp,
    PATH: `${paths.shimDir}${path.delimiter}${base.PATH ?? ""}`,
  };
}

function quote(text) {
  return `'${text.replaceAll("'", "'\\''")}'`;
}

/**
 * A stand-in `systemctl` for Linux. The app runs `ts install` at startup and
 * `toka restart` stops and starts the service; unshimmed, an isolated instance
 * would stop the user's real indexer. The shim logs each call and acts
 * only on the isolated indexer: `stop` ends it and `start` respawns it if
 * it is not running, as systemd would. Every other call is a no-op that
 * succeeds.
 */
export function shimScript(paths) {
  return `#!/bin/sh
# Installed by scripts/isolated: isolated Toka must never control the real
# toka-search.service. It controls the isolated indexer instead.
PIDFILE=${quote(paths.indexerPidFile)}
printf '%s systemctl %s\\n' "$(date -Is)" "$*" >> ${quote(paths.shimLog)}
pid=$(cat "$PIDFILE" 2>/dev/null)
# A stale pid file may name a recycled PID: only trust it if it is our indexer.
if [ -n "$pid" ] && { ! tr '\\0' '\\n' < "/proc/$pid/cmdline" 2>/dev/null | head -n 1 | grep -qxF ${quote(paths.tsBinary)} || ! tr '\\0' '\\n' < "/proc/$pid/environ" 2>/dev/null | grep -qxF ${quote(`XDG_CONFIG_HOME=${paths.config}`)}; }; then
  pid=
fi
case "$*" in
  *"stop toka-search.service"*)
    if [ -n "$pid" ]; then
      kill -s TERM -- "-$pid" 2>/dev/null
      # Like the real "systemctl stop", return only once the indexer is gone.
      n=0
      while kill -s 0 -- "-$pid" 2>/dev/null && [ "$n" -lt 100 ]; do
        sleep 0.1
        n=$((n + 1))
      done
      kill -s KILL -- "-$pid" 2>/dev/null
    fi
    ;;
  *"start toka-search.service"*)
    if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
      setsid ${quote(paths.tsBinary)} daemon run >> ${quote(paths.indexerLog)} 2>&1 </dev/null &
      echo $! > "$PIDFILE"
    fi
    ;;
esac
exit 0
`;
}

/**
 * A stand-in `loginctl`. `ts install` on Linux probes lingering with
 * `show-user $USER -p Linger --value` and enables it otherwise; unshimmed,
 * an isolated instance would change the user's real login state. The probe
 * passes `--value`, so it answers the bare `yes` the real tool prints (and
 * the probe compares against), reporting lingering as already enabled.
 * Every other call is a no-op that succeeds.
 */
export function loginctlShimScript() {
  return `#!/bin/sh
# Installed by scripts/isolated: isolated Toka must never change host login
# state. Report lingering as already enabled; do nothing otherwise.
case "$*" in
  *"show-user"*)
    echo yes
    ;;
esac
exit 0
`;
}

/**
 * A stand-in `launchctl` for macOS. `ts install` calls `launchctl bootstrap`,
 * `launchctl print`, `launchctl bootout`, and `launchctl kickstart`; unshimmed,
 * an isolated instance would manage the user's real launchd agent. The shim
 * logs each call and acts only on the isolated indexer:
 * - `bootout` / `bootstrap + kickstart` start/stop the isolated indexer.
 * - `print` reports the agent as bootstrapped and running (so `ts install`
 *   thinks the service is already installed and healthy, skipping re-install).
 * Every other call is a no-op that succeeds.
 */
export function launchctlShimScript(paths) {
  return `#!/bin/sh
# Installed by scripts/isolated (macOS): isolated Toka must never manage the
# real launchd agent. It controls the isolated indexer instead.
PIDFILE=${quote(paths.indexerPidFile)}
printf '%s launchctl %s\\n' "$(date)" "$*" >> ${quote(paths.launchctlShimLog)}
pid=$(cat "$PIDFILE" 2>/dev/null)
case "$1" in
  print)
    # Report the agent as bootstrapped and running so ts install short-circuits.
    printf 'state = running\\n'
    exit 0
    ;;
  bootout)
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill -s TERM -- "-$pid" 2>/dev/null
      n=0
      while kill -s 0 -- "-$pid" 2>/dev/null && [ "$n" -lt 100 ]; do
        sleep 0.1
        n=$((n + 1))
      done
      kill -s KILL -- "-$pid" 2>/dev/null
    fi
    ;;
  bootstrap|kickstart)
    if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
      ${quote(paths.tsBinary)} daemon run >> ${quote(paths.indexerLog)} 2>&1 &
      echo $! > "$PIDFILE"
    fi
    ;;
esac
exit 0
`;
}

/** The index-folders.json an isolated home starts with. */
export function seedConfig(folderPaths, newId) {
  return {
    folders: folderPaths.map((folderPath) => ({
      id: newId(),
      path: folderPath,
      mount: null,
    })),
  };
}

export function indexerArguments() {
  return ["daemon", "run"];
}

/**
 * How the isolated build compiles the `ts` search helper. It runs from
 * `src-tauri` because the tauri CLI builds from there too (it sets its
 * working directory to the project dir before invoking cargo), so a
 * relative `CARGO_TARGET_DIR` resolves under `src-tauri` for both
 * binaries — matching `isolatedPaths`, and landing `ts` where `launch`
 * looks. An absolute target dir is unaffected by the cwd either way.
 */
export function tsBuildInvocation(paths) {
  return {
    command: "cargo",
    args: ["build", "--release", "-p", "ts"],
    cwd: path.join(paths.worktree, "src-tauri"),
  };
}

/**
 * True when `pid` is alive and running `binary`.  A PID recorded earlier may
 * since belong to an unrelated process, so the command line is checked before
 * anything is signalled.
 *
 * On Linux: reads /proc/<pid>/cmdline (the null-delimited argv[0]).
 * On macOS: uses `ps -p <pid> -o comm=` (the executable base name).
 */
export function isProcessOf(pid, binary, procRoot = "/proc") {
  if (!Number.isInteger(pid) || pid <= 1) return false;
  if (process.platform === "linux") {
    try {
      const commandLine = readFileSync(
        path.join(procRoot, String(pid), "cmdline"),
        "utf8",
      );
      return commandLine.split("\0")[0] === binary;
    } catch {
      return false;
    }
  }
  // macOS: compare the full argv[0] from `ps -p <pid> -o args=`
  // shortcut: `ps -o args=` truncates long paths at ~1024 chars; upgrade to
  // sysctl proc_pidpathinfo if that ever becomes a problem.
  try {
    const result = spawnSync("ps", ["-p", String(pid), "-o", "args="], {
      encoding: "utf8",
    });
    if (result.status !== 0) return false;
    const argv0 = result.stdout.trim().split(/\s+/)[0];
    return argv0 === binary;
  } catch {
    return false;
  }
}

/**
 * True when `pid` is `paths.binary` running with this worktree's isolated
 * config. The environment check keeps one worktree from signalling another's
 * app when they share a cargo target directory.
 */
export function isIsolatedProcess(pid, paths, procRoot = "/proc") {
  return isIsolatedBinary(pid, paths, paths.binary, procRoot);
}

/**
 * True when `pid` is the worktree's isolated `ts` indexer. The indexer is
 * a different binary than the app now, so it gets its own check.
 */
export function isIsolatedIndexerProcess(pid, paths, procRoot = "/proc") {
  return isIsolatedBinary(pid, paths, paths.tsBinary, procRoot);
}

function isIsolatedBinary(pid, paths, binary, procRoot) {
  if (!isProcessOf(pid, binary, procRoot)) return false;
  if (process.platform === "linux") {
    try {
      const environment = readFileSync(
        path.join(procRoot, String(pid), "environ"),
        "utf8",
      ).split("\0");
      return environment.includes(`XDG_CONFIG_HOME=${paths.config}`);
    } catch {
      return false;
    }
  }
  // macOS: read env via `ps ewww -p <pid>` (env appended after args, space-separated).
  // shortcut: very long env strings may be truncated by ps; a production fix
  // would use sysctl KERN_PROCARGS2 via a small native helper.
  try {
    const result = spawnSync("ps", ["ewww", "-p", String(pid)], {
      encoding: "utf8",
    });
    if (result.status !== 0) return false;
    // ps ewww output: header line, then `pid tt stat time command env...`
    // The environment key=value pairs follow the command on the same line.
    const line = result.stdout.split("\n").find((l) => {
      const trimmed = l.trimStart();
      return trimmed.startsWith(String(pid) + " ") || trimmed.startsWith(String(pid) + "\t");
    });
    if (!line) return false;
    return line.includes(`XDG_CONFIG_HOME=${paths.config}`);
  } catch {
    return false;
  }
}

/** The live indexer PID: the pid file wins, since the shim may have respawned it. */
export function currentIndexerPid(paths, state) {
  try {
    const pid = Number.parseInt(readFileSync(paths.indexerPidFile, "utf8"), 10);
    if (Number.isInteger(pid)) return pid;
  } catch {
    /* no pid file yet */
  }
  return state?.indexerPid ?? null;
}

/**
 * True when `pid` is alive and is the node process running `scriptPath`.
 *
 * On Linux: reads /proc/<pid>/cmdline.
 * On macOS: uses `ps -p <pid> -o args=`, falling back to a liveness check
 * when `ps` is unavailable (e.g. sandboxed environment).
 */
export function isSupervisorOf(pid, scriptPath, procRoot = "/proc") {
  if (!Number.isInteger(pid) || pid <= 1) return false;
  if (process.platform === "linux") {
    try {
      const parts = readFileSync(
        path.join(procRoot, String(pid), "cmdline"),
        "utf8",
      ).split("\0");
      // A foreground launch may have been started with a relative path.
      const cwd = readlinkSync(path.join(procRoot, String(pid), "cwd"));
      return parts.some(
        (part) =>
          part.endsWith(".mjs") &&
          (part === scriptPath || path.resolve(cwd, part) === scriptPath),
      );
    } catch {
      return false;
    }
  }
  // macOS: check liveness first; verify identity when ps is available.
  // shortcut: if ps is unavailable, we fall back to a plain liveness check,
  // which is weaker (a recycled PID looks alive), but safe for the supervisor
  // polling loop — upgrade to sysctl KERN_PROCARGS2 if stricter checks needed.
  try {
    process.kill(pid, 0);
  } catch {
    return false; // process is gone
  }
  try {
    const result = spawnSync("ps", ["-p", String(pid), "-o", "args="], {
      encoding: "utf8",
    });
    if (result.error) {
      // ps unavailable — we know the PID is alive; treat it as the supervisor.
      return true;
    }
    if (result.status !== 0) return false;
    const args = result.stdout.trim().split(/\s+/);
    return args.some(
      (part) =>
        part.endsWith(".mjs") &&
        (part === scriptPath || path.resolve(part) === scriptPath),
    );
  } catch {
    // ps threw — PID is alive (checked above), treat as supervisor.
    return true;
  }
}

export function readState(paths) {
  try {
    return JSON.parse(readFileSync(paths.state, "utf8"));
  } catch {
    return null;
  }
}

/** What is actually running for a worktree, judged from live processes. */
export function describeRun(paths, state = readState(paths)) {
  const indexerPid = state ? currentIndexerPid(paths, state) : null;
  const app = state ? isIsolatedProcess(state.appPid, paths) : false;
  const indexer = state ? isIsolatedIndexerProcess(indexerPid, paths) : false;
  return {
    worktree: paths.worktree,
    name: paths.name,
    home: paths.home,
    binary: paths.binary,
    built: existsSync(paths.binary),
    appPid: state?.appPid ?? null,
    indexerPid,
    appRunning: app,
    indexerRunning: indexer,
    running: app || indexer,
    startedAt: state?.startedAt ?? null,
  };
}

export function gitWorktrees(cwd) {
  const result = spawnSync("git", ["worktree", "list", "--porcelain"], {
    cwd,
    encoding: "utf8",
  });
  if (result.status !== 0) return [];
  return result.stdout
    .split("\n")
    .filter((line) => line.startsWith("worktree "))
    .map((line) => line.slice("worktree ".length));
}

export function gitToplevel(cwd) {
  const result = spawnSync("git", ["rev-parse", "--show-toplevel"], {
    cwd,
    encoding: "utf8",
  });
  if (result.status !== 0) {
    throw new Error(`${cwd} is not inside a git worktree.`);
  }
  return result.stdout.trim();
}
