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
 * Copies `sourcePath` to `~/.local/opt/toka/<installedName>` and registers a
 * desktop entry for it. The copy lands on a temporary name first, because
 * writing over the executable of a running instance fails with ETXTBSY.
 */
async function installExecutable({
  sourcePath,
  installedName,
  homeDir,
  iconPath,
  plocatePath,
  updatedbPath,
  plocateSourcePath,
  plocateLicensePath,
  runServiceCommand,
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

  if (plocatePath && updatedbPath) {
    const libexecDir = path.join(installDir, "libexec");
    await mkdir(libexecDir, { recursive: true });
    for (const [source, name] of [
      [plocatePath, "toka-plocate"],
      [updatedbPath, "toka-updatedb"],
    ]) {
      const destination = path.join(libexecDir, name);
      await copyFile(source, `${destination}.new`);
      await chmod(`${destination}.new`, 0o755);
      await rename(`${destination}.new`, destination);
    }
  }

  if (plocateSourcePath && plocateLicensePath) {
    const documentationDir = path.join(
      homeDir,
      ".local",
      "share",
      "doc",
      "toka",
    );
    await mkdir(documentationDir, { recursive: true });
    await copyFile(
      plocateSourcePath,
      path.join(documentationDir, "plocate-1.1.24.tar.gz"),
    );
    await copyFile(
      plocateLicensePath,
      path.join(documentationDir, "plocate-COPYING"),
    );
  }

  const binDir = path.join(homeDir, ".local", "bin");
  await mkdir(binDir, { recursive: true });
  const commandLink = path.join(binDir, "toka");
  await unlink(`${commandLink}.new`).catch((error) => {
    if (error.code !== "ENOENT") throw error;
  });
  await symlink(installedPath, `${commandLink}.new`);
  await rename(`${commandLink}.new`, commandLink);

  const userServiceDir = path.join(homeDir, ".config", "systemd", "user");
  await mkdir(userServiceDir, { recursive: true });
  const service = `[Unit]
Description=Toka private media indexer

[Service]
ExecStart=\"${installedPath.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}\" --indexer
Restart=on-failure
RestartSec=2
Nice=10
IOSchedulingClass=idle

[Install]
WantedBy=default.target
`;
  await writeFile(
    path.join(userServiceDir, "toka-indexer.service"),
    service,
    "utf8",
  );
  runServiceCommand?.("daemon-reload");
  runServiceCommand?.("enable", "toka-indexer.service");
  // `enable --now` leaves an already-running service on the old executable
  // inode after an atomic app update. Restart starts first installs too, and
  // guarantees updates immediately run the newly installed indexer.
  runServiceCommand?.("restart", "toka-indexer.service");

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
Exec=${desktopExecPath(installedPath)} %U
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

const runServiceCommand = (...arguments_) => {
  const result = spawnSync("systemctl", ["--user", ...arguments_], {
    stdio: "inherit",
  });
  if (result.error)
    console.warn(
      `Could not run systemctl --user ${arguments_.join(" ")}: ${result.error.message}`,
    );
};

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  const homeDir = process.env.HOME ?? os.homedir();
  const iconPath = path.resolve("src-tauri/icons/128x128.png");

  if (process.argv.includes("--appimage")) {
    await installAppImage({
      bundleDir: path.join(releaseDir(), "bundle", "appimage"),
      homeDir,
      iconPath,
      runServiceCommand,
    });
  } else {
    await installBinary({
      binaryPath: path.join(releaseDir(), "toka"),
      homeDir,
      iconPath,
      plocatePath: path.join(releaseDir(), "toka-plocate"),
      updatedbPath: path.join(releaseDir(), "toka-updatedb"),
      plocateSourcePath: path.resolve(
        "src-tauri/resources/plocate-1.1.24.tar.gz",
      ),
      plocateLicensePath: path.resolve("src-tauri/resources/plocate-COPYING"),
      runServiceCommand,
    });
  }

  console.log("Installed Toka for the current user.");
}
