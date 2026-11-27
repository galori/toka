import { spawn, spawnSync } from "node:child_process";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import {
  indexerArguments,
  isIsolatedIndexerProcess,
  isProcessOf,
  isolatedEnv,
  isolatedPaths,
  launchctlShimScript,
  loginctlShimScript,
  parseArguments,
  seedConfig,
  shimScript,
  tsBuildInvocation,
} from "./lib.mjs";

describe("parseArguments", () => {
  it("reads a command with boolean and value flags", () => {
    expect(
      parseArguments(["launch", "--detach", "--seed-from=media", "--no-seed"]),
    ).toEqual({
      command: "launch",
      flags: { detach: true, "seed-from": "media", "no-seed": true },
    });
    expect(parseArguments(["stop", "--worktree", "/w"]).flags).toEqual({
      worktree: "/w",
    });
  });

  it("rejects unknown commands, unknown flags and stray words", () => {
    expect(() => parseArguments(["install"])).toThrow(/Unknown command/);
    expect(() => parseArguments(["launch", "--bogus"])).toThrow(/Unknown flag/);
    expect(() => parseArguments(["launch", "extra"])).toThrow(/Unexpected/);
    expect(() => parseArguments(["launch", "--detach=1"])).toThrow(
      /takes no value/,
    );
    expect(() => parseArguments(["stop", "--worktree"])).toThrow(
      /needs a value/,
    );
  });

  it("shows help without a command", () => {
    expect(parseArguments([]).command).toBe("help");
  });
});

describe("isolatedPaths and isolatedEnv", () => {
  const paths = isolatedPaths("/repo/.worktrees/feature", {});

  it("keeps every location under the worktree's .scratch folder", () => {
    for (const key of ["home", "config", "data", "cache", "shim", "state"]) {
      expect(
        (paths as Record<string, string>)[key].startsWith(
          "/repo/.worktrees/feature/.scratch/isolated-home",
        ),
      ).toBe(true);
    }
    expect(paths.binary).toBe(
      "/repo/.worktrees/feature/src-tauri/target/release/toka",
    );
    expect(paths.name).toBe("feature");
  });

  it("follows CARGO_TARGET_DIR for the binary", () => {
    expect(
      isolatedPaths("/w", { CARGO_TARGET_DIR: "/shared/target" }).binary,
    ).toBe("/shared/target/release/toka");
  });

  it("points HOME and the XDG directories at the isolated home and puts the shim first on PATH", () => {
    const env = isolatedEnv(paths, { PATH: "/usr/bin", HOME: "/home/u" });
    expect(env.HOME).toBe(paths.home);
    expect(env.XDG_CONFIG_HOME).toBe(paths.config);
    expect(env.XDG_DATA_HOME).toBe(paths.data);
    expect(env.XDG_CACHE_HOME).toBe(paths.cache);
    expect(env.TMPDIR).toBe(paths.tmp);
    expect(env.PATH).toBe(`${paths.shimDir}:/usr/bin`);
  });

  it("shadows both systemctl and loginctl in the shim dir", () => {
    expect(paths.shim).toBe(path.join(paths.shimDir, "systemctl"));
    expect(paths.loginctlShim).toBe(path.join(paths.shimDir, "loginctl"));
  });

  it("shadows launchctl in the shim dir", () => {
    expect(paths.launchctlShim).toBe(path.join(paths.shimDir, "launchctl"));
  });

  it("runs the indexer as ts daemon run", () => {
    expect(indexerArguments()).toEqual(["daemon", "run"]);
  });

  it("builds ts from src-tauri, where a relative CARGO_TARGET_DIR lands beside the app binary", () => {
    const targetDir = "shared-target";
    const withTarget = isolatedPaths("/w", { CARGO_TARGET_DIR: targetDir });
    const build = tsBuildInvocation(withTarget);

    expect(build.command).toBe("cargo");
    expect(build.args).toEqual(["build", "--release", "-p", "ts"]);
    // The tauri CLI builds from src-tauri, so a relative target dir
    // resolves under it; the ts build runs from the same directory, or
    // its binary lands where `launch` never looks.
    expect(build.cwd).toBe(path.join("/w", "src-tauri"));
    expect(path.join(build.cwd, targetDir, "release", "ts")).toBe(
      withTarget.tsBinary,
    );
  });

  it("keeps the ts binary beside the app binary", () => {
    expect(paths.tsBinary).toBe(
      "/repo/.worktrees/feature/src-tauri/target/release/ts",
    );
    expect(
      isolatedPaths("/w", { CARGO_TARGET_DIR: "/shared/target" }).tsBinary,
    ).toBe("/shared/target/release/ts");
  });
});

describe("shimScript and seedConfig", () => {
  it("logs and succeeds instead of touching systemd", async () => {
    await mkdir(path.resolve(".scratch"), { recursive: true });
    const dir = await mkdtemp(path.resolve(".scratch/shim-test-"));
    try {
      const shim = path.join(dir, "systemctl");
      const log = path.join(dir, "it's.log");
      await writeFile(
        shim,
        shimScript({ ...isolatedPaths(dir, {}), shimLog: log }),
      );
      await chmod(shim, 0o755);
      const result = spawnSync(shim, ["--user", "stop", "toka-search.service"]);
      expect(result.status).toBe(0);
      expect(await readFile(log, "utf8")).toContain(
        "systemctl --user stop toka-search.service",
      );
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("reports lingering as enabled instead of changing host login state", async () => {
    await mkdir(path.resolve(".scratch"), { recursive: true });
    const dir = await mkdtemp(path.resolve(".scratch/loginctl-test-"));
    try {
      const shim = path.join(dir, "loginctl");
      await writeFile(shim, loginctlShimScript());
      await chmod(shim, 0o755);
      // The probe `ts install` runs, `--value` and all.
      const probe = spawnSync(
        shim,
        ["show-user", "u", "-p", "Linger", "--value"],
        {
          encoding: "utf8",
        },
      );
      expect(probe.status).toBe(0);
      expect(probe.stdout).toBe("yes\n");
      const enable = spawnSync(shim, ["enable-linger", "u"], {
        encoding: "utf8",
      });
      expect(enable.status).toBe(0);
      expect(enable.stdout).toBe("");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("launchctl shim logs, reports the agent running, and never signals a PID that is not the isolated indexer", async () => {
    await mkdir(path.resolve(".scratch"), { recursive: true });
    const dir = await mkdtemp(path.resolve(".scratch/launchctl-test-"));
    // An unrelated process whose PID a stale pid file names.
    const bystander = spawn("sleep", ["30"], { detached: true });
    try {
      const pidFile = path.join(dir, "indexer.pid");
      const log = path.join(dir, "it's.log");
      await writeFile(pidFile, `${bystander.pid}\n`);
      const shim = path.join(dir, "launchctl");
      await writeFile(
        shim,
        launchctlShimScript({
          ...isolatedPaths(dir, {}),
          indexerPidFile: pidFile,
          launchctlShimLog: log,
        }),
      );
      await chmod(shim, 0o755);
      const print = spawnSync(shim, ["print", "gui/501/app.toka.ts"], {
        encoding: "utf8",
      });
      expect(print.status).toBe(0);
      expect(print.stdout).toContain("state = running");
      const bootout = spawnSync(shim, ["bootout", "gui/501/app.toka.ts"]);
      expect(bootout.status).toBe(0);
      expect(() => process.kill(bystander.pid!, 0)).not.toThrow();
      expect(await readFile(log, "utf8")).toContain(
        "launchctl bootout gui/501/app.toka.ts",
      );
    } finally {
      bystander.kill("SIGKILL");
      await rm(dir, { recursive: true, force: true });
    }
  });

  it("builds a folder list with fresh ids and no mount identity", () => {
    let count = 0;
    expect(seedConfig(["/a", "/b"], () => `id${(count += 1)}`)).toEqual({
      folders: [
        { id: "id1", path: "/a", mount: null },
        { id: "id2", path: "/b", mount: null },
      ],
    });
  });
});

describe("isProcessOf", () => {
  it("is false for PIDs that are not that binary, so a reused PID is never signalled", () => {
    expect(isProcessOf(process.pid, "/not/toka")).toBe(false);
    expect(isProcessOf(0, process.execPath)).toBe(false);
    expect(isProcessOf(999999999, process.execPath)).toBe(false);
  });
});

describe.skipIf(process.platform !== "linux")(
  "toka-isolated launch, status and stop",
  () => {
    const cli = path.resolve("scripts/isolated/toka-isolated.mjs");
    let repo: string;
    let env: NodeJS.ProcessEnv;

    const run = (...args: string[]) =>
      spawnSync(process.execPath, [cli, ...args, "--worktree", repo], {
        encoding: "utf8",
        env,
      });

    beforeEach(async () => {
      await mkdir(path.resolve(".scratch"), { recursive: true });
      repo = await mkdtemp(path.resolve(".scratch/isolated-test-"));
      spawnSync("git", ["init", "-q"], { cwd: repo });
      await mkdir(path.join(repo, "test/fixtures"), { recursive: true });
      await writeFile(path.join(repo, "test/fixtures/a.mp4"), "x");
      // Stand-ins for the real binaries: each records its environment and
      // arguments, then idles like the app and the indexer do.
      const target = path.join(repo, "target");
      await mkdir(path.join(target, "release"), { recursive: true });
      const fake = `#!/bin/bash
exec -a "$0" node "$(dirname "$0")/fake.mjs" "$@"
`;
      // Re-exec node under each binary's own name, as a compiled binary
      // appears in /proc/<pid>/cmdline.
      for (const name of ["toka", "ts"]) {
        const binary = path.join(target, "release", name);
        await writeFile(binary, fake);
        await chmod(binary, 0o755);
      }
      await writeFile(
        path.join(target, "release/fake.mjs"),
        `import fs from "node:fs";
const role = process.argv[2] === "daemon" ? "indexer" : "app";
fs.writeFileSync(process.env.XDG_DATA_HOME + "/" + role + ".json",
  JSON.stringify({ argv: process.argv.slice(2), env: process.env }));
setInterval(() => {}, 1000);
`,
      );
      env = { ...process.env, CARGO_TARGET_DIR: target };
      delete env.TOKA_ISOLATED_SUPERVISOR;
    });

    afterEach(async () => {
      run("stop", "--clean");
      await rm(repo, { recursive: true, force: true });
    });

    it("runs app and indexer with isolated env, seeds a copy, and stops both with no orphans", async () => {
      const launched = run("launch", "--detach");
      expect(launched.status, launched.stderr).toBe(0);

      const status = JSON.parse(run("status", "--json").stdout);
      const here = status.find(
        (s: { worktree: string }) => s.worktree === repo,
      );
      expect(here.appRunning).toBe(true);
      expect(here.indexerRunning).toBe(true);
      const { appPid, indexerPid } = here;

      const home = path.join(repo, ".scratch/isolated-home");
      await vi.waitFor(async () => {
        await stat(path.join(home, "data/app.json"));
        await stat(path.join(home, "data/indexer.json"));
      });
      const indexer = JSON.parse(
        await readFile(path.join(home, "data/indexer.json"), "utf8"),
      );
      expect(indexer.argv).toEqual(["daemon", "run"]);
      expect(indexer.env.XDG_CONFIG_HOME).toBe(path.join(home, "config"));
      expect(indexer.env.HOME).toBe(home);
      expect(indexer.env.PATH.startsWith(path.join(home, "bin"))).toBe(true);

      const config = JSON.parse(
        await readFile(
          path.join(home, "config/toka/index-folders.json"),
          "utf8",
        ),
      );
      expect(config.folders[0].path).toBe(path.join(home, "fixtures"));
      await stat(path.join(home, "fixtures/a.mp4"));
      // Both service shims wait on PATH, so a startup `ts install`
      // cannot reach the host's systemd or login state.
      await stat(path.join(home, "bin/systemctl"));
      await stat(path.join(home, "bin/loginctl"));

      expect(run("launch", "--detach").status).not.toBe(0);

      const stopped = run("stop");
      expect(stopped.status, stopped.stderr).toBe(0);
      for (const pid of [appPid, indexerPid]) {
        expect(() => process.kill(pid, 0)).toThrow();
      }
      expect(JSON.parse(run("status", "--json").stdout)[0].running).toBe(false);

      expect(run("stop", "--clean").status).toBe(0);
      await expect(stat(home)).rejects.toThrow();
    }, 30000);

    it("lets the app's systemctl stop and start act on the isolated indexer only, and cleans up the respawned one", async () => {
      expect(run("launch", "--detach").status).toBe(0);
      const home = path.join(repo, ".scratch/isolated-home");
      const shim = path.join(home, "bin/systemctl");
      const first = Number(
        (await readFile(path.join(home, "run/indexer.pid"), "utf8")).trim(),
      );

      spawnSync(shim, ["--user", "stop", "toka-search.service"]);
      await vi.waitFor(() => expect(() => process.kill(first, 0)).toThrow());

      spawnSync(shim, ["--user", "start", "toka-search.service"], {
        env: isolatedEnv(isolatedPaths(repo, env), env),
      });
      const second = Number(
        (await readFile(path.join(home, "run/indexer.pid"), "utf8")).trim(),
      );
      expect(second).not.toBe(first);
      await vi.waitFor(() =>
        expect(isIsolatedIndexerProcess(second, isolatedPaths(repo, env))).toBe(
          true,
        ),
      );
      expect(() => process.kill(second, 0)).not.toThrow();

      expect(run("stop").status).toBe(0);
      await vi.waitFor(() => expect(() => process.kill(second, 0)).toThrow());
    }, 30000);

    it("refuses to launch when nothing has been built", () => {
      env.CARGO_TARGET_DIR = path.join(repo, "missing");
      const result = run("launch");
      expect(result.status).toBe(1);
      expect(result.stderr).toMatch(/No built binaries at .+ and .+\. Run: /);
    });

    it("refuses to launch when the ts binary is missing", async () => {
      await rm(path.join(repo, "target/release/ts"));
      const result = run("launch");
      expect(result.status).toBe(1);
      expect(result.stderr).toMatch(/No built binaries at .+ and .+\. Run: /);
    });
  },
);

describe.skipIf(process.platform !== "darwin")(
  "toka-isolated launch, status and stop (macOS)",
  () => {
    const cli = path.resolve("scripts/isolated/toka-isolated.mjs");
    let repo: string;
    let env: NodeJS.ProcessEnv;

    const run = (...args: string[]) =>
      spawnSync(process.execPath, [cli, ...args, "--worktree", repo], {
        encoding: "utf8",
        env,
      });

    beforeEach(async () => {
      await mkdir(path.resolve(".scratch"), { recursive: true });
      repo = await mkdtemp(path.resolve(".scratch/isolated-test-darwin-"));
      spawnSync("git", ["init", "-q"], { cwd: repo });
      await mkdir(path.join(repo, "test/fixtures"), { recursive: true });
      await writeFile(path.join(repo, "test/fixtures/a.mp4"), "x");
      // Stand-ins for the real binaries. On macOS, `ps -p <pid> -o args=`
      // returns the full argv[0], so we use a plain node script whose
      // process.argv[0] is the binary path (node does this automatically
      // when exec'd via the wrapper below, via exec -a).
      const target = path.join(repo, "target");
      await mkdir(path.join(target, "release"), { recursive: true });
      // On macOS, /bin/bash exec -a sets argv[0] to the given name, which
      // is the full path; that is what `ps -o args=` returns.
      const fake = `#!/bin/bash
exec -a "$0" node "$(dirname "$0")/fake.mjs" "$@"
`;
      for (const name of ["toka", "ts"]) {
        const binary = path.join(target, "release", name);
        await writeFile(binary, fake);
        await chmod(binary, 0o755);
      }
      await writeFile(
        path.join(target, "release/fake.mjs"),
        `import fs from "node:fs";
const role = process.argv[2] === "daemon" ? "indexer" : "app";
fs.writeFileSync(process.env.XDG_DATA_HOME + "/" + role + ".json",
  JSON.stringify({ argv: process.argv.slice(2), env: process.env }));
setInterval(() => {}, 1000);
`,
      );
      env = { ...process.env, CARGO_TARGET_DIR: target };
      delete env.TOKA_ISOLATED_SUPERVISOR;
    });

    afterEach(async () => {
      run("stop", "--clean");
      await rm(repo, { recursive: true, force: true });
    });

    it("runs app and indexer with isolated env, seeds a copy, and stops both with no orphans", async () => {
      const launched = run("launch", "--detach");
      expect(launched.status, launched.stderr).toBe(0);

      const status = JSON.parse(run("status", "--json").stdout);
      const here = status.find(
        (s: { worktree: string }) => s.worktree === repo,
      );
      expect(here.appRunning).toBe(true);
      expect(here.indexerRunning).toBe(true);
      const { appPid, indexerPid } = here;

      const home = path.join(repo, ".scratch/isolated-home");
      await vi.waitFor(async () => {
        await stat(path.join(home, "data/app.json"));
        await stat(path.join(home, "data/indexer.json"));
      });
      const indexer = JSON.parse(
        await readFile(path.join(home, "data/indexer.json"), "utf8"),
      );
      expect(indexer.argv).toEqual(["daemon", "run"]);
      expect(indexer.env.XDG_CONFIG_HOME).toBe(path.join(home, "config"));
      expect(indexer.env.HOME).toBe(home);
      expect(indexer.env.PATH.startsWith(path.join(home, "bin"))).toBe(true);

      const config = JSON.parse(
        await readFile(
          path.join(home, "config/toka/index-folders.json"),
          "utf8",
        ),
      );
      expect(config.folders[0].path).toBe(path.join(home, "fixtures"));
      await stat(path.join(home, "fixtures/a.mp4"));
      // macOS shim: launchctl (not systemctl/loginctl)
      await stat(path.join(home, "bin/launchctl"));

      expect(run("launch", "--detach").status).not.toBe(0);

      const stopped = run("stop");
      expect(stopped.status, stopped.stderr).toBe(0);
      for (const pid of [appPid, indexerPid]) {
        expect(() => process.kill(pid, 0)).toThrow();
      }
      expect(JSON.parse(run("status", "--json").stdout)[0].running).toBe(false);

      expect(run("stop", "--clean").status).toBe(0);
      await expect(stat(home)).rejects.toThrow();
    }, 30000);

    it("lets the app's launchctl bootout and bootstrap act on the isolated indexer only, and cleans up the respawned one", async () => {
      expect(run("launch", "--detach").status).toBe(0);
      const home = path.join(repo, ".scratch/isolated-home");
      const shim = path.join(home, "bin/launchctl");
      const first = Number(
        (await readFile(path.join(home, "run/indexer.pid"), "utf8")).trim(),
      );

      spawnSync(shim, ["bootout", "gui/501/app.toka.ts"]);
      await vi.waitFor(() => expect(() => process.kill(first, 0)).toThrow());

      spawnSync(shim, ["bootstrap", "gui/501", "app.toka.ts.plist"], {
        env: isolatedEnv(isolatedPaths(repo, env), env),
      });
      const second = Number(
        (await readFile(path.join(home, "run/indexer.pid"), "utf8")).trim(),
      );
      expect(second).not.toBe(first);
      await vi.waitFor(() =>
        expect(isIsolatedIndexerProcess(second, isolatedPaths(repo, env))).toBe(
          true,
        ),
      );

      expect(run("stop").status).toBe(0);
      await vi.waitFor(() => expect(() => process.kill(second, 0)).toThrow());
    }, 30000);

    it("refuses to launch when nothing has been built", () => {
      env.CARGO_TARGET_DIR = path.join(repo, "missing");
      const result = run("launch");
      expect(result.status).toBe(1);
      expect(result.stderr).toMatch(/No built binaries at .+ and .+\. Run: /);
    });

    it("refuses to launch when the ts binary is missing", async () => {
      await rm(path.join(repo, "target/release/ts"));
      const result = run("launch");
      expect(result.status).toBe(1);
      expect(result.stderr).toMatch(/No built binaries at .+ and .+\. Run: /);
    });
  },
);
