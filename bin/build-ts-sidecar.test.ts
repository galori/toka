import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  stat,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  buildTsSidecar,
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

  it("merges the bundle overlay in every flow that packages the app", async () => {
    const buildScript = await readFile(path.resolve("bin/build.mjs"), "utf8");
    const ci = await readFile(path.resolve(".github/workflows/ci.yml"), "utf8");
    const release = await readFile(
      path.resolve(".github/workflows/release.yml"),
      "utf8",
    );

    for (const flow of [buildScript, ci, release]) {
      expect(flow).toContain("tauri.bundle.conf.json");
      expect(flow).toContain("build-ts-sidecar.mjs");
    }
    // Compile and bundle stay split: the overlay reaches only
    // `tauri bundle`, never a compile whose build script would
    // mis-copy the sidecar under cargo's nested build layout.
    expect(buildScript).toContain('"bundle"');
    expect(ci).toContain("tauri bundle");
    expect(release).toContain("tauri bundle");
  });
});
