import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DEFAULT_APP_BUNDLE = "src-tauri/target/release/bundle/macos/Toka.app";
const DEFAULT_APPLICATIONS_DIR = "/Applications";

function needsCommandLink(commandLink, commandTarget) {
  try {
    const details = fs.lstatSync(commandLink);
    if (!details.isSymbolicLink()) {
      throw new Error(
        `Shell command path already exists and is not a Toka link: ${commandLink}`,
      );
    }

    const existingTarget = path.resolve(
      path.dirname(commandLink),
      fs.readlinkSync(commandLink),
    );
    if (existingTarget !== commandTarget) {
      throw new Error(
        `Shell command path already points to another target: ${commandLink}`,
      );
    }
    return false;
  } catch (error) {
    if (error?.code === "ENOENT") return true;
    throw error;
  }
}

export function installMacosApp({
  source = process.env.TOKA_APP_BUNDLE ?? DEFAULT_APP_BUNDLE,
  applicationsDir = process.env.TOKA_APPLICATIONS_DIR ??
    DEFAULT_APPLICATIONS_DIR,
  homeDir = os.homedir(),
} = {}) {
  const resolvedSource = path.resolve(source);
  const resolvedApplicationsDir = path.resolve(applicationsDir);
  const resolvedHomeDir = path.resolve(homeDir);
  const destination = path.join(
    resolvedApplicationsDir,
    path.basename(resolvedSource),
  );

  if (!fs.existsSync(resolvedSource)) {
    throw new Error(`App bundle not found: ${resolvedSource}`);
  }

  if (resolvedSource === destination) {
    throw new Error(`App bundle is already in ${resolvedApplicationsDir}`);
  }

  const commandLink = path.join(resolvedHomeDir, ".local", "bin", "toka");
  const commandTarget = path.join(destination, "Contents", "MacOS", "toka");
  const createCommandLink = needsCommandLink(commandLink, commandTarget);

  fs.mkdirSync(resolvedApplicationsDir, { recursive: true });
  fs.rmSync(destination, { recursive: true, force: true });

  try {
    fs.renameSync(resolvedSource, destination);
  } catch (error) {
    if (error?.code !== "EXDEV") {
      throw error;
    }

    fs.cpSync(resolvedSource, destination, { recursive: true });
    fs.rmSync(resolvedSource, { recursive: true, force: true });
  }

  if (createCommandLink) {
    fs.mkdirSync(path.dirname(commandLink), { recursive: true });
    fs.symlinkSync(commandTarget, commandLink);
  }

  return destination;
}

const entrypoint = process.argv[1] ? path.resolve(process.argv[1]) : "";

if (entrypoint === fileURLToPath(import.meta.url)) {
  try {
    const destination = installMacosApp();
    console.log(`Installed ${destination}`);
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
