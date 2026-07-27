import { chmod, mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { installAppImage } from "./install-linux.mjs";

describe("linux build script", () => {
  it("uses the provenance-aware user-local AppImage install path by default", async () => {
    const packageJson = JSON.parse(
      await readFile(path.resolve("package.json"), "utf8"),
    );
    const buildScript = await readFile(path.resolve("bin/build.mjs"), "utf8");
    const installScript = await readFile(path.resolve("scripts/install-linux.mjs"), "utf8");

    expect(packageJson.scripts["build:linux"]).toBe("node bin/build.mjs linux");
    expect(buildScript).toContain('target === "linux" ? "appimage" : "app,dmg"');
    expect(buildScript).toContain('"scripts/install-linux.mjs"');
    expect(buildScript).not.toContain('"sudo"');
    expect(installScript).toContain("await rename(temporaryAppImage, installedAppImage)");
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

    await expect(readFile(installedAppImage, "utf8")).resolves.toBe("test appimage");
    await expect(readFile(desktopEntry, "utf8")).resolves.toContain(
      `Exec=${installedAppImage} %U`,
    );
  });
});
