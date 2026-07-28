import { chmod, copyFile, mkdir, readdir, rename, writeFile } from "node:fs/promises";
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
async function installExecutable({ sourcePath, installedName, homeDir, iconPath }) {
  const installDir = path.join(homeDir, ".local", "opt", "toka");
  const applicationsDir = path.join(homeDir, ".local", "share", "applications");
  const iconsDir = path.join(homeDir, ".local", "share", "icons", "hicolor", "128x128", "apps");
  const installedPath = path.join(installDir, installedName);
  const temporaryPath = `${installedPath}.new`;

  await mkdir(installDir, { recursive: true });
  await mkdir(applicationsDir, { recursive: true });
  await copyFile(sourcePath, temporaryPath);
  await chmod(temporaryPath, 0o755);
  await rename(temporaryPath, installedPath);

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
Comment=Search and play videos indexed by your operating system
Exec=${desktopExecPath(installedPath)} %U
Icon=${iconName}
Terminal=false
Type=Application
Categories=AudioVideo;Video;
StartupWMClass=app.toka.desktop
`;

  await writeFile(path.join(applicationsDir, desktopFileName), desktopEntry, "utf8");
}

export async function installAppImage({ bundleDir, homeDir, iconPath }) {
  const appImages = (await readdir(bundleDir))
    .filter((fileName) => fileName.startsWith(`${appName}_`) && fileName.endsWith(".AppImage"))
    .sort();

  if (appImages.length === 0) {
    throw new Error(`No ${appName} AppImage found in ${bundleDir}`);
  }

  await installExecutable({
    sourcePath: path.join(bundleDir, appImages.at(-1)),
    installedName: `${appName}.AppImage`,
    homeDir,
    iconPath,
  });
}

export async function installBinary({ binaryPath, homeDir, iconPath }) {
  await installExecutable({ sourcePath: binaryPath, installedName: appName, homeDir, iconPath });
}

export function releaseDir(env = process.env) {
  return path.join(env.CARGO_TARGET_DIR ?? path.resolve("src-tauri/target"), "release");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const homeDir = process.env.HOME ?? os.homedir();
  const iconPath = path.resolve("src-tauri/icons/128x128.png");

  if (process.argv.includes("--appimage")) {
    await installAppImage({
      bundleDir: path.join(releaseDir(), "bundle", "appimage"),
      homeDir,
      iconPath,
    });
  } else {
    await installBinary({ binaryPath: path.join(releaseDir(), "toka"), homeDir, iconPath });
  }

  console.log("Installed Toka for the current user.");
}
