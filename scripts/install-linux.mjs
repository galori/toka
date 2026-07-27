import { chmod, copyFile, mkdir, readdir, rename, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const appName = "Toka";
const desktopFileName = "toka.desktop";

function desktopExecPath(filePath) {
  return filePath.replaceAll("\\", "\\\\").replaceAll(" ", "\\ ");
}

export async function installAppImage({ bundleDir, homeDir, iconPath }) {
  const appImages = (await readdir(bundleDir))
    .filter((fileName) => fileName.startsWith(`${appName}_`) && fileName.endsWith(".AppImage"))
    .sort();

  if (appImages.length === 0) {
    throw new Error(`No ${appName} AppImage found in ${bundleDir}`);
  }

  const installDir = path.join(homeDir, ".local", "opt", "toka");
  const applicationsDir = path.join(homeDir, ".local", "share", "applications");
  const iconsDir = path.join(homeDir, ".local", "share", "icons", "hicolor", "128x128", "apps");
  const installedAppImage = path.join(installDir, `${appName}.AppImage`);
  const temporaryAppImage = path.join(installDir, `${appName}.AppImage.new`);

  await mkdir(installDir, { recursive: true });
  await mkdir(applicationsDir, { recursive: true });
  await copyFile(path.join(bundleDir, appImages.at(-1)), temporaryAppImage);
  await chmod(temporaryAppImage, 0o755);
  await rename(temporaryAppImage, installedAppImage);

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
Exec=${desktopExecPath(installedAppImage)} %U
Icon=${iconName}
Terminal=false
Type=Application
Categories=AudioVideo;Video;
StartupWMClass=app.toka.desktop
`;

  await writeFile(path.join(applicationsDir, desktopFileName), desktopEntry, "utf8");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await installAppImage({
    bundleDir: path.resolve("src-tauri/target/release/bundle/appimage"),
    homeDir: process.env.HOME ?? os.homedir(),
    iconPath: path.resolve("src-tauri/icons/128x128.png"),
  });
  console.log("Installed Toka for the current user.");
}
