import { spawnSync } from "node:child_process";
import {
  chmod,
  copyFile,
  mkdir,
  readdir,
  rename,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const appName = "Toka";
const desktopFileName = "toka.desktop";

function desktopExecPath(filePath) {
  return filePath.replaceAll("\\", "\\\\").replaceAll(" ", "\\ ");
}

/**
 * Atomically replaces `linkPath` with a link to `target`, staging through
 * a `.new` name first.
 */
async function replaceLink(linkPath, target) {
  await unlink(`${linkPath}.new`).catch((error) => {
    if (error.code !== "ENOENT") throw error;
  });
  await symlink(target, `${linkPath}.new`);
  await rename(`${linkPath}.new`, linkPath);
}

/**
 * Copies `sourcePath` to `~/.local/opt/toka/<installedName>` and registers a
 * desktop entry for it. The copy lands on a temporary name first, because
 * writing over the executable of a running instance fails with ETXTBSY.
 */
async function installExecutable({
  sourcePath,
  installedName,
  homeDir,
  iconPath,
  tsSourcePath,
  runTsInstall,
}) {
  const installDir = path.join(homeDir, ".local", "opt", "toka");
  const applicationsDir = path.join(homeDir, ".local", "share", "applications");
  const iconsDir = path.join(
    homeDir,
    ".local",
    "share",
    "icons",
    "hicolor",
    "128x128",
    "apps",
  );
  const installedPath = path.join(installDir, installedName);
  const temporaryPath = `${installedPath}.new`;

  await mkdir(installDir, { recursive: true });
  await mkdir(applicationsDir, { recursive: true });
  await copyFile(sourcePath, temporaryPath);
  await chmod(temporaryPath, 0o755);
  await rename(temporaryPath, installedPath);

  const binDir = path.join(homeDir, ".local", "bin");
  await mkdir(binDir, { recursive: true });
  await replaceLink(path.join(binDir, "toka"), installedPath);

  // The search helper beside the app, linked for the shell too. The app
  // resolves it as its sibling; the link is for interactive use.
  const installedTsPath = path.join(installDir, "ts");
  let tsInstalled = false;
  if (tsSourcePath === undefined) {
    console.warn("No ts binary provided; skipping the search helper install.");
  } else {
    try {
      const tsTemporaryPath = `${installedTsPath}.new`;
      await copyFile(tsSourcePath, tsTemporaryPath);
      await chmod(tsTemporaryPath, 0o755);
      await rename(tsTemporaryPath, installedTsPath);
      await replaceLink(path.join(binDir, "ts"), installedTsPath);
      tsInstalled = true;
    } catch (error) {
      console.warn(`Could not install the ts search helper: ${error.message}`);
    }
  }

  // The service belongs to `ts` now: `ts install` writes the unit,
  // migrates the old toka-indexer.service away, and starts the daemon —
  // idempotently, so updates and first installs share this path. A failed
  // service setup warns but never fails the install: the app runs
  // degraded and retries at startup.
  if (tsInstalled) {
    runTsInstall?.(installedTsPath);
  }

  let iconName = "application-x-executable";
  try {
    await mkdir(iconsDir, { recursive: true });
    await copyFile(iconPath, path.join(iconsDir, "toka.png"));
    iconName = "toka";
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }

  const desktopEntry = `[Desktop Entry]
Name=${appName}
Comment=Search and play videos in your Toka folders
Exec=${desktopExecPath(installedPath)} %F
Icon=${iconName}
Terminal=false
Type=Application
Categories=AudioVideo;Video;
StartupWMClass=app.toka.desktop
`;

  await writeFile(
    path.join(applicationsDir, desktopFileName),
    desktopEntry,
    "utf8",
  );
}

export async function installAppImage({ bundleDir, ...installation }) {
  const appImages = (await readdir(bundleDir))
    .filter(
      (fileName) =>
        fileName.startsWith(`${appName}_`) && fileName.endsWith(".AppImage"),
    )
    .sort();

  if (appImages.length === 0) {
    throw new Error(`No ${appName} AppImage found in ${bundleDir}`);
  }

  await installExecutable({
    sourcePath: path.join(bundleDir, appImages.at(-1)),
    installedName: `${appName}.AppImage`,
    ...installation,
  });
}

export async function installBinary(options) {
  const { binaryPath, ...installation } = options;
  await installExecutable({
    sourcePath: binaryPath,
    installedName: appName,
    ...installation,
  });
}

export function releaseDir(env = process.env) {
  return path.join(
    env.CARGO_TARGET_DIR ?? path.resolve("src-tauri/target"),
    "release",
  );
}

const runTsInstall = (installedTsPath) => {
  const result = spawnSync(installedTsPath, ["install"], {
    stdio: "inherit",
  });
  if (result.error) {
    console.warn(
      `Could not run ts install: ${result.error.message}. The app will retry at startup.`,
    );
  } else if (result.status !== 0) {
    console.warn(
      `ts install exited with ${result.status}. The app will retry at startup.`,
    );
  }
};

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  const homeDir = process.env.HOME ?? os.homedir();
  const iconPath = path.resolve("src-tauri/icons/128x128.png");
  // Built alongside the app by bin/build.mjs.
  const tsSourcePath = path.join(releaseDir(), "ts");

  if (process.argv.includes("--appimage")) {
    await installAppImage({
      bundleDir: path.join(releaseDir(), "bundle", "appimage"),
      homeDir,
      iconPath,
      tsSourcePath,
      runTsInstall,
    });
  } else {
    await installBinary({
      binaryPath: path.join(releaseDir(), "toka"),
      homeDir,
      iconPath,
      tsSourcePath,
      runTsInstall,
    });
  }

  console.log("Installed Toka for the current user.");
}
