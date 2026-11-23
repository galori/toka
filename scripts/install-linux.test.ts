import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  readlink,
  stat,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  installAppImage,
  installBinary,
  releaseDir,
  runTsInstall,
} from "./install-linux.mjs";

describe("releaseDir", () => {
  it("defaults to the crate's own target directory", () => {
    expect(releaseDir({})).toBe(path.resolve("src-tauri/target/release"));
  });

  it("follows CARGO_TARGET_DIR, so a shared build cache still installs the right binary", () => {
    expect(releaseDir({ CARGO_TARGET_DIR: "/shared/target" })).toBe(
      "/shared/target/release",
    );
  });
});

describe("linux build script", () => {
  it("skips AppImage bundling by default, because a local install needs no bundled libraries", async () => {
    const packageJson = JSON.parse(
      await readFile(path.resolve("package.json"), "utf8"),
    );
    const buildScript = await readFile(path.resolve("bin/build.mjs"), "utf8");
    const installScript = await readFile(
      path.resolve("scripts/install-linux.mjs"),
      "utf8",
    );

    expect(packageJson.scripts["build:linux"]).toBe("node bin/build.mjs linux");
    expect(packageJson.scripts["build:linux:install"]).toBe(
      "node bin/build.mjs linux --install",
    );
    expect(packageJson.scripts["build:mac:install"]).toBe(
      "node bin/build.mjs mac --install",
    );
    expect(buildScript).toContain("--no-bundle");
    expect(buildScript).toContain('"scripts/install-linux.mjs"');
    expect(buildScript).not.toContain('"sudo"');
    expect(installScript).toContain(
      "await rename(temporaryPath, installedPath)",
    );
  });

  it("still packages an AppImage when one is asked for", async () => {
    const buildScript = await readFile(path.resolve("bin/build.mjs"), "utf8");

    expect(buildScript).toContain("--appimage");
    expect(buildScript).toContain("appimage");
  });
});

describe("installAppImage", () => {
  it("installs the AppImage and registers it for the current user", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-linux-install-"));
    const bundleDir = path.join(root, "bundle", "appimage");
    const homeDir = path.join(root, "home");
    const appImagePath = path.join(bundleDir, "Toka_0.1.0_amd64.AppImage");

    await mkdir(bundleDir, { recursive: true });
    await writeFile(appImagePath, "test appimage");
    await chmod(appImagePath, 0o755);

    await installAppImage({
      bundleDir,
      homeDir,
      iconPath: path.join(root, "128x128.png"),
    });

    const installedAppImage = path.join(
      homeDir,
      ".local",
      "opt",
      "toka",
      "Toka.AppImage",
    );
    const desktopEntry = path.join(
      homeDir,
      ".local",
      "share",
      "applications",
      "toka.desktop",
    );

    await expect(readFile(installedAppImage, "utf8")).resolves.toBe(
      "test appimage",
    );
    await expect(readFile(desktopEntry, "utf8")).resolves.toContain(
      `Exec=${installedAppImage} %F`,
    );
  });
});

describe("runTsInstall", () => {
  const writeFakeTs = async (dir: string, behavior: string) => {
    const tsPath = path.join(dir, "ts");
    const logPath = path.join(dir, "calls.log");
    await writeFile(
      tsPath,
      `#!/bin/sh\necho "$1" >> "${logPath}"\n${behavior}\n`,
    );
    await chmod(tsPath, 0o755);
    return { tsPath, logPath };
  };
  const calls = (logPath: string) =>
    readFile(logPath, "utf8").then((text) =>
      text.split("\n").filter((line) => line.length > 0),
    );

  it("restarts the indexer after a successful install, so updates pick up the new binary", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-restart-"));
    const { tsPath, logPath } = await writeFakeTs(root, "exit 0");

    runTsInstall(tsPath);

    expect(await calls(logPath)).toEqual(["install", "restart"]);
  });

  it("skips the restart when the install itself failed", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-restart-"));
    const { tsPath, logPath } = await writeFakeTs(
      root,
      'if [ "$1" = install ]; then exit 1; fi\nexit 0',
    );

    runTsInstall(tsPath);

    expect(await calls(logPath)).toEqual(["install"]);
  });

  it("warns but never throws when the restart fails", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-restart-"));
    const { tsPath, logPath } = await writeFakeTs(
      root,
      'if [ "$1" = restart ]; then exit 1; fi\nexit 0',
    );

    expect(() => runTsInstall(tsPath)).not.toThrow();
    expect(await calls(logPath)).toEqual(["install", "restart"]);
  });
});

describe("installBinary", () => {
  it("installs the ts helper beside the app and sets up its service", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-linux-install-"));
    const releaseDir = path.join(root, "release");
    const homeDir = path.join(root, "home");
    const binaryPath = path.join(releaseDir, "toka");
    const tsSourcePath = path.join(releaseDir, "ts");
    const tsInstalls: string[] = [];
    await mkdir(releaseDir, { recursive: true });
    await writeFile(binaryPath, "test binary");
    await writeFile(tsSourcePath, "test ts");

    await installBinary({
      binaryPath,
      homeDir,
      iconPath: path.join(root, "128x128.png"),
      tsSourcePath,
      runTsInstall: (installedTsPath: string) =>
        tsInstalls.push(installedTsPath),
    });

    const installedTs = path.join(homeDir, ".local", "opt", "toka", "ts");
    expect(await readlink(path.join(homeDir, ".local", "bin", "toka"))).toBe(
      path.join(homeDir, ".local", "opt", "toka", "Toka"),
    );
    expect(await readlink(path.join(homeDir, ".local", "bin", "ts"))).toBe(
      installedTs,
    );
    await expect(readFile(installedTs, "utf8")).resolves.toBe("test ts");
    // No unit file of the installer's own: `ts install` owns the service
    // now, including migrating the old unit away.
    await expect(
      readFile(
        path.join(
          homeDir,
          ".config",
          "systemd",
          "user",
          "toka-indexer.service",
        ),
        "utf8",
      ),
    ).rejects.toThrow();
    expect(tsInstalls).toEqual([installedTs]);
  });

  it("skips the service setup when no ts binary is provided", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-linux-no-ts-"));
    const releaseDir = path.join(root, "release");
    const homeDir = path.join(root, "home");
    const binaryPath = path.join(releaseDir, "toka");
    const tsInstalls: string[] = [];
    await mkdir(releaseDir, { recursive: true });
    await writeFile(binaryPath, "test binary");

    await installBinary({
      binaryPath,
      homeDir,
      iconPath: path.join(root, "128x128.png"),
      runTsInstall: (installedTsPath: string) =>
        tsInstalls.push(installedTsPath),
    });

    expect(tsInstalls).toEqual([]);
    await expect(
      readlink(path.join(homeDir, ".local", "bin", "ts")),
    ).rejects.toThrow();
  });

  it("installs the unbundled executable and registers it for the current user", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-linux-binary-"));
    const releaseDir = path.join(root, "release");
    const homeDir = path.join(root, "home");
    const binaryPath = path.join(releaseDir, "toka");

    await mkdir(releaseDir, { recursive: true });
    await writeFile(binaryPath, "test binary");
    await chmod(binaryPath, 0o755);

    await installBinary({
      binaryPath,
      homeDir,
      iconPath: path.join(root, "128x128.png"),
    });

    const installedBinary = path.join(homeDir, ".local", "opt", "toka", "Toka");
    const desktopEntry = path.join(
      homeDir,
      ".local",
      "share",
      "applications",
      "toka.desktop",
    );

    await expect(readFile(installedBinary, "utf8")).resolves.toBe(
      "test binary",
    );
    expect((await stat(installedBinary)).mode & 0o111).toBeTruthy();
    await expect(readFile(desktopEntry, "utf8")).resolves.toContain(
      `Exec=${installedBinary} %F`,
    );
  });

  it("replaces a running install atomically rather than writing over a busy executable", async () => {
    const root = await mkdtemp(
      path.join(os.tmpdir(), "toka-linux-binary-replace-"),
    );
    const releaseDir = path.join(root, "release");
    const homeDir = path.join(root, "home");
    const installDir = path.join(homeDir, ".local", "opt", "toka");
    const binaryPath = path.join(releaseDir, "toka");

    await mkdir(releaseDir, { recursive: true });
    await mkdir(installDir, { recursive: true });
    await writeFile(path.join(installDir, "Toka"), "previous binary");
    await writeFile(binaryPath, "next binary");

    await installBinary({
      binaryPath,
      homeDir,
      iconPath: path.join(root, "128x128.png"),
    });

    await expect(readFile(path.join(installDir, "Toka"), "utf8")).resolves.toBe(
      "next binary",
    );
  });
});
