import { spawnSync } from "node:child_process";
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
  isIsolatedProcess,
  isProcessOf,
  isolatedEnv,
  isolatedPaths,
  parseArguments,
  seedConfig,
  shimScript,
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

  it("points the XDG directories at the isolated home and puts the shim first on PATH", () => {
    const env = isolatedEnv(paths, { PATH: "/usr/bin", HOME: "/home/u" });
    expect(env.XDG_CONFIG_HOME).toBe(paths.config);
    expect(env.XDG_DATA_HOME).toBe(paths.data);
    expect(env.XDG_CACHE_HOME).toBe(paths.cache);
    expect(env.TMPDIR).toBe(paths.tmp);
    expect(env.PATH).toBe(`${paths.shimDir}:/usr/bin`);
    expect(env.HOME).toBe("/home/u");
  });

  it("runs the indexer with --indexer", () => {
    expect(indexerArguments()).toEqual(["--indexer"]);
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
      const result = spawnSync(shim, [
        "--user",
        "stop",
        "toka-indexer.service",
      ]);
      expect(result.status).toBe(0);
      expect(await readFile(log, "utf8")).toContain(
        "systemctl --user stop toka-indexer.service",
      );
    } finally {
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
      // A stand-in for the real binary: records its environment and arguments,
      // then idles like the app and the indexer do.
      const target = path.join(repo, "target");
      await mkdir(path.join(target, "release"), { recursive: true });
      const fake = path.join(target, "release/toka");
      // Re-exec node under the binary's own name, as a compiled binary appears
      // in /proc/<pid>/cmdline.
      await writeFile(
        fake,
        `#!/bin/bash
exec -a "$0" node "$(dirname "$0")/fake.mjs" "$@"
`,
      );
      await writeFile(
        path.join(target, "release/fake.mjs"),
        `import fs from "node:fs";
const role = process.argv[2] === "--indexer" ? "indexer" : "app";
fs.writeFileSync(process.env.XDG_DATA_HOME + "/" + role + ".json",
  JSON.stringify({ argv: process.argv.slice(2), env: process.env }));
setInterval(() => {}, 1000);
`,
      );
      await chmod(fake, 0o755);
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
      expect(indexer.argv).toEqual(["--indexer"]);
      expect(indexer.env.XDG_CONFIG_HOME).toBe(path.join(home, "config"));
      expect(indexer.env.PATH.startsWith(path.join(home, "bin"))).toBe(true);

      const config = JSON.parse(
        await readFile(
          path.join(home, "config/toka/index-folders.json"),
          "utf8",
        ),
      );
      expect(config.folders[0].path).toBe(path.join(home, "fixtures"));
      await stat(path.join(home, "fixtures/a.mp4"));

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

      spawnSync(shim, ["--user", "stop", "toka-indexer.service"]);
      await vi.waitFor(() => expect(() => process.kill(first, 0)).toThrow());

      spawnSync(shim, ["--user", "start", "toka-indexer.service"], {
        env: isolatedEnv(isolatedPaths(repo, env), env),
      });
      const second = Number(
        (await readFile(path.join(home, "run/indexer.pid"), "utf8")).trim(),
      );
      expect(second).not.toBe(first);
      await vi.waitFor(() =>
        expect(isIsolatedProcess(second, isolatedPaths(repo, env))).toBe(true),
      );
      expect(() => process.kill(second, 0)).not.toThrow();

      expect(run("stop").status).toBe(0);
      await vi.waitFor(() => expect(() => process.kill(second, 0)).toThrow());
    }, 30000);

    it("refuses to launch when nothing has been built", () => {
      env.CARGO_TARGET_DIR = path.join(repo, "missing");
      const result = run("launch");
      expect(result.status).toBe(1);
      expect(result.stderr).toMatch(/No built binary/);
    });
  },
);
