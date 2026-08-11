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
      `Exec=${installedAppImage} %U`,
    );
  });
});

describe("installBinary", () => {
  it("installs Toka's search tools, PATH link, and background indexer", async () => {
    const root = await mkdtemp(
      path.join(os.tmpdir(), "toka-linux-owned-index-"),
    );
    const releaseDir = path.join(root, "release");
    const homeDir = path.join(root, "home");
    const binaryPath = path.join(releaseDir, "toka");
    const plocatePath = path.join(releaseDir, "toka-plocate");
    const updatedbPath = path.join(releaseDir, "toka-updatedb");
    const serviceCommands: string[][] = [];
    await mkdir(releaseDir, { recursive: true });
    await Promise.all([
      writeFile(binaryPath, "test binary"),
      writeFile(plocatePath, "bundled plocate"),
      writeFile(updatedbPath, "bundled updatedb"),
    ]);

    await installBinary({
      binaryPath,
      homeDir,
      iconPath: path.join(root, "128x128.png"),
      plocatePath,
      updatedbPath,
      runServiceCommand: (...arguments_) => serviceCommands.push(arguments_),
    });

    await expect(
      readFile(
        path.join(homeDir, ".local", "opt", "toka", "libexec", "toka-plocate"),
        "utf8",
      ),
    ).resolves.toBe("bundled plocate");
    await expect(
      readFile(
        path.join(homeDir, ".local", "opt", "toka", "libexec", "toka-updatedb"),
        "utf8",
      ),
    ).resolves.toBe("bundled updatedb");
    expect(await readlink(path.join(homeDir, ".local", "bin", "toka"))).toBe(
      path.join(homeDir, ".local", "opt", "toka", "Toka"),
    );
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
    ).resolves.toContain(
      `ExecStart="${path.join(homeDir, ".local", "opt", "toka", "Toka")}" --indexer`,
    );
    expect(serviceCommands).toEqual([
      ["daemon-reload"],
      ["enable", "toka-indexer.service"],
      ["restart", "toka-indexer.service"],
    ]);
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
      `Exec=${installedBinary} %U`,
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
