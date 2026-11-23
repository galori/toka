import { spawnSync } from "node:child_process";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import {
  buildTsSidecar,
  isDirectExecution,
  parseSidecarArgs,
  resolveTriple,
  sidecarStagePath,
  tsReleasePath,
} from "./build-ts-sidecar.mjs";

describe("parseSidecarArgs", () => {
  it("defaults to the host target", () => {
    expect(parseSidecarArgs([])).toEqual({ target: undefined });
  });

  it("takes an explicit cargo target triple", () => {
    expect(parseSidecarArgs(["--target", "aarch64-apple-darwin"])).toEqual({
      target: "aarch64-apple-darwin",
    });
  });

  it("rejects unknown options and a missing triple", () => {
    expect(() => parseSidecarArgs(["--install"])).toThrow(/Usage/);
    expect(() => parseSidecarArgs(["--target"])).toThrow(/Usage/);
  });
});

describe("resolveTriple", () => {
  it("passes an explicit target through without probing rustc", () => {
    const exec = () => {
      throw new Error("must not probe rustc");
    };

    expect(resolveTriple({ target: "x86_64-unknown-linux-gnu" }, exec)).toBe(
      "x86_64-unknown-linux-gnu",
    );
  });

  it("asks rustc for the host triple otherwise", () => {
    const exec = () => ({ status: 0, stdout: "aarch64-apple-darwin\n" });

    expect(resolveTriple({ target: undefined }, exec)).toBe(
      "aarch64-apple-darwin",
    );
  });

  it("fails clearly when rustc cannot say", () => {
    const exec = () => ({ status: 1, stdout: "" });

    expect(() => resolveTriple({ target: undefined }, exec)).toThrow(
      /Could not determine the target triple/,
    );
  });
});

describe("sidecar paths", () => {
  const root = path.resolve("worktree-root");

  it("reads the ts binary out of the release directory", () => {
    expect(tsReleasePath({ cargoTarget: undefined }, root, {})).toBe(
      path.join(root, "src-tauri", "target", "release", "ts"),
    );
  });

  it("reads a cross-compiled binary out of its target subdirectory", () => {
    expect(
      tsReleasePath({ cargoTarget: "aarch64-apple-darwin" }, root, {}),
    ).toBe(
      path.join(
        root,
        "src-tauri",
        "target",
        "aarch64-apple-darwin",
        "release",
        "ts",
      ),
    );
  });

  it("follows CARGO_TARGET_DIR, like the installers do", () => {
    expect(
      tsReleasePath({ cargoTarget: "host-triple" }, root, {
        CARGO_TARGET_DIR: "/shared/target",
      }),
    ).toBe(path.join("/shared/target", "host-triple", "release", "ts"));
  });

  it("stages the sidecar where Tauri looks for it", () => {
    expect(sidecarStagePath("aarch64-apple-darwin", root)).toBe(
      path.join(root, "src-tauri", "binaries", "ts-aarch64-apple-darwin"),
    );
  });
});

describe("buildTsSidecar", () => {
  const setupReleaseTs = async (root: string, cargoTarget?: string) => {
    const releaseDir = path.join(
      root,
      "src-tauri",
      "target",
      ...(cargoTarget === undefined ? [] : [cargoTarget]),
      "release",
    );
    await mkdir(releaseDir, { recursive: true });
    const binary = path.join(releaseDir, "ts");
    await writeFile(binary, "test ts");
    await chmod(binary, 0o755);
  };

  it("builds the release ts and stages it under its target triple", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-sidecar-"));
    await setupReleaseTs(root);
    const spawned: string[][] = [];
    const spawn = (command: string, args: string[]) => {
      spawned.push([command, ...args]);
      return { status: 0 };
    };

    await buildTsSidecar({
      triple: "host-triple",
      cargoTarget: undefined,
      buildVersion: "0.1.1+1.abc1234",
      gitSha: "abc1234",
      root,
      env: {},
      spawn,
    });

    expect(spawned).toEqual([["cargo", "build", "--release", "-p", "ts"]]);
    const staged = path.join(root, "src-tauri", "binaries", "ts-host-triple");
    await expect(readFile(staged, "utf8")).resolves.toBe("test ts");
    expect((await stat(staged)).mode & 0o111).toBeTruthy();
  });

  it("cross-compiles and forwards provenance to the ts build", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-sidecar-"));
    await setupReleaseTs(root, "aarch64-apple-darwin");
    const seen: Record<string, string | undefined>[] = [];
    const spawn = (
      command: string,
      args: string[],
      options: { env: Record<string, string> },
    ) => {
      expect(command).toBe("cargo");
      expect(args).toEqual([
        "build",
        "--release",
        "-p",
        "ts",
        "--target",
        "aarch64-apple-darwin",
      ]);
      seen.push({
        TS_BUILD_VERSION: options.env.TS_BUILD_VERSION,
        TOKA_GIT_SHA: options.env.TOKA_GIT_SHA,
      });
      return { status: 0 };
    };

    await buildTsSidecar({
      triple: "aarch64-apple-darwin",
      cargoTarget: "aarch64-apple-darwin",
      buildVersion: "0.1.1+9.deadbee",
      gitSha: "deadbee",
      root,
      env: {},
      spawn,
    });

    expect(seen).toEqual([
      { TS_BUILD_VERSION: "0.1.1+9.deadbee", TOKA_GIT_SHA: "deadbee" },
    ]);
  });

  it("fails the build when cargo does", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-sidecar-"));
    const spawn = () => ({ status: 1 });

    await expect(
      buildTsSidecar({
        triple: "host-triple",
        cargoTarget: undefined,
        buildVersion: "0.1.1",
        gitSha: "unknown",
        root,
        env: {},
        spawn,
      }),
    ).rejects.toThrow(/cargo build -p ts failed/);
  });
});

describe("direct execution guard", () => {
  it("runs the script when invoked through a symlinked path", async () => {
    const root = await mkdtemp(
      path.join(os.tmpdir(), "toka-ts-sidecar-guard-"),
    );
    const link = path.join(root, "build-ts-sidecar.mjs");
    await symlink(path.resolve("bin/build-ts-sidecar.mjs"), link);

    // Invalid args exercise the guard without building: a reached main
    // rejects them, while a missed guard exits 0 staging nothing.
    for (const invoked of [link, path.resolve("bin/build-ts-sidecar.mjs")]) {
      const run = spawnSync("node", [invoked, "--bogus"], {
        encoding: "utf8",
      });
      expect(run.status).not.toBe(0);
      expect(run.stderr).toMatch(/Usage/);
    }
  });

  it("compares real paths, not spellings", async () => {
    const root = await mkdtemp(
      path.join(os.tmpdir(), "toka-ts-sidecar-guard-"),
    );
    const real = path.join(root, "real.mjs");
    await writeFile(real, "");
    const link = path.join(root, "link.mjs");
    await symlink(real, link);

    expect(isDirectExecution(pathToFileURL(real).href, link)).toBe(true);
    expect(
      isDirectExecution(pathToFileURL(real).href, path.join(root, "other.mjs")),
    ).toBe(false);
    expect(isDirectExecution(pathToFileURL(real).href, undefined)).toBe(false);
  });
});

// Compile and bundle stay split on purpose: the bundle overlay (which
// carries the sidecar) reaches only `tauri bundle`, never a compile whose
// build script would mis-copy the sidecar under cargo's nested per-package
// build layout. These checkers assert that split shape — a reunited
// single-phase `tauri build --bundles … --config …bundle.conf.json` must
// fail them even though it contains every magic string.
function splitViolationsBuildScript(buildScript: string): string[] {
  const code = buildScript.replace(/\/\/.*$/gm, "");
  const violations: string[] = [];
  const argvLists = [...code.matchAll(/\[([\s\S]*?)\]/g)]
    .map((match) => match[1])
    .filter((body) => body.includes('"tauri"'));
  if (
    !argvLists.some(
      (body) => body.includes('"build"') && body.includes('"--no-bundle"'),
    )
  ) {
    violations.push("no `tauri build` argv with --no-bundle");
  }
  for (const body of argvLists.filter((item) => item.includes('"build"'))) {
    for (const flag of [
      '"--bundles"',
      '"--config"',
      "tauri.bundle.conf.json",
    ]) {
      if (body.includes(flag)) {
        violations.push(`a \`tauri build\` argv carries ${flag}`);
      }
    }
  }
  if (
    !argvLists.some(
      (body) =>
        body.includes('"bundle"') && body.includes("tauri.bundle.conf.json"),
    )
  ) {
    violations.push("no `tauri bundle` argv merges the overlay");
  }
  const outsideArgv = code.replace(/\[[^\]]*\]/gs, "");
  if (outsideArgv.includes("tauri.bundle.conf.json")) {
    violations.push("the overlay path appears outside a spawn argv");
  }
  return violations;
}

function splitViolationsWorkflow(workflow: string): string[] {
  const lines = workflow.replace(/^\s*#.*$/gm, "").split("\n");
  const violations: string[] = [];
  const buildLines = lines.filter((line) => line.includes("tauri build"));
  const bundleLines = lines.filter((line) => line.includes("tauri bundle"));
  if (!buildLines.some((line) => line.includes("--no-bundle"))) {
    violations.push("no `tauri build` line with --no-bundle");
  }
  for (const line of buildLines) {
    for (const flag of ["--bundles", "--config", "tauri.bundle.conf.json"]) {
      if (line.includes(flag)) {
        violations.push(`a \`tauri build\` line carries ${flag}`);
      }
    }
  }
  if (
    !bundleLines.some(
      (line) =>
        line.includes("--bundles") &&
        line.includes("--config") &&
        line.includes("tauri.bundle.conf.json"),
    )
  ) {
    violations.push("no `tauri bundle` line merges the overlay");
  }
  for (const line of lines) {
    if (
      line.includes("tauri.bundle.conf.json") &&
      !line.includes("tauri bundle")
    ) {
      violations.push("the overlay path appears off a `tauri bundle` line");
    }
  }
  return violations;
}

describe("tauri sidecar bundling", () => {
  it("ships ts in every bundle, where the app resolves it as its sibling", async () => {
    const overlay = JSON.parse(
      await readFile(path.resolve("src-tauri/tauri.bundle.conf.json"), "utf8"),
    );
    const base = JSON.parse(
      await readFile(path.resolve("src-tauri/tauri.conf.json"), "utf8"),
    );

    expect(overlay.bundle.externalBin).toContain("binaries/ts");
    // The overlay stays out of the base config on purpose: tauri-build
    // fails any compile whose sidecar file is missing, so only bundle
    // builds merge it in — plain cargo builds never need a staged ts.
    expect(base.bundle.externalBin).toBeUndefined();
  });

  it("declares the moreutils conflict on the deb that ships /usr/bin/ts", async () => {
    const base = JSON.parse(
      await readFile(path.resolve("src-tauri/tauri.conf.json"), "utf8"),
    );

    // The bundler copies external binaries beside the app binary, so the
    // deb installs the ts sidecar as /usr/bin/ts — colliding with
    // moreutils' /usr/bin/ts. Without Conflicts, dpkg fails both install
    // orders; the AppImage and dmg carry no such control field and keep
    // their sidecar inside the bundle.
    expect(base.bundle.linux.deb.conflicts).toContain("moreutils");
  });

  it("merges the bundle overlay in every flow that packages the app", async () => {
    const buildScript = await readFile(path.resolve("bin/build.mjs"), "utf8");
    const ci = await readFile(path.resolve(".github/workflows/ci.yml"), "utf8");
    const release = await readFile(
      path.resolve(".github/workflows/release.yml"),
      "utf8",
    );

    for (const flow of [buildScript, ci, release]) {
      expect(flow).toContain("build-ts-sidecar.mjs");
    }
    // Compile and bundle stay split: the overlay reaches only
    // `tauri bundle`, never a compile whose build script would
    // mis-copy the sidecar under cargo's nested build layout.
    expect(buildScript).toContain('"--no-bundle"');
    expect(splitViolationsBuildScript(buildScript)).toEqual([]);
    expect(splitViolationsWorkflow(ci)).toEqual([]);
    expect(splitViolationsWorkflow(release)).toEqual([]);
  });

  it("rejects a reunited single-phase flow that bundles during compile", () => {
    // The split-shape test above must fail if compile+bundle ever reunite:
    // these fixtures carry every magic string yet violate the split.
    const reunitedArgv =
      'spawnSync("npm", ["run", "tauri", "build", "--", "--bundles", "app", "--config", "src-tauri/tauri.bundle.conf.json"])';
    expect(splitViolationsBuildScript(reunitedArgv).length).toBeGreaterThan(0);
    const reunitedShell =
      "npm run tauri build -- --bundles app --config src-tauri/tauri.bundle.conf.json";
    expect(splitViolationsWorkflow(reunitedShell).length).toBeGreaterThan(0);
  });
});
