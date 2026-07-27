import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DEFAULT_APP_BUNDLE = "src-tauri/target/release/bundle/macos/Toka.app";
const DEFAULT_APPLICATIONS_DIR = "/Applications";

export function installMacosApp({
  source = process.env.TOKA_APP_BUNDLE ?? DEFAULT_APP_BUNDLE,
  applicationsDir = process.env.TOKA_APPLICATIONS_DIR ?? DEFAULT_APPLICATIONS_DIR,
} = {}) {
  const resolvedSource = path.resolve(source);
  const resolvedApplicationsDir = path.resolve(applicationsDir);
  const destination = path.join(resolvedApplicationsDir, path.basename(resolvedSource));

  if (!fs.existsSync(resolvedSource)) {
    throw new Error(`App bundle not found: ${resolvedSource}`);
  }

  if (resolvedSource === destination) {
    throw new Error(`App bundle is already in ${resolvedApplicationsDir}`);
  }

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
