import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function gitSha() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" });
  if (result.status !== 0) throw result.error ?? new Error("git rev-parse HEAD failed");
  return result.stdout.trim();
}

function bumpPatch(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!match) throw new Error(`Cannot bump non-semver version: ${version}`);
  return `${match[1]}.${match[2]}.${Number(match[3]) + 1}`;
}

function replaceVersion(path, pattern, version) {
  const source = readFileSync(path, "utf8");
  const updated = source.replace(pattern, `$1${version}$3`);
  if (updated === source) throw new Error(`Could not update version in ${path}`);
  writeFileSync(path, updated);
}

export function prepareBuild() {
  const packagePath = join(root, "package.json");
  const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
  const version = bumpPatch(packageJson.version);
  packageJson.version = version;
  writeFileSync(packagePath, `${JSON.stringify(packageJson, null, 2)}\n`);

  const lockPath = join(root, "package-lock.json");
  const lock = JSON.parse(readFileSync(lockPath, "utf8"));
  lock.version = version;
  lock.packages[""].version = version;
  writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`);

  replaceVersion(join(root, "src-tauri/tauri.conf.json"), /("version": ")([^\"]+)(")/, version);
  replaceVersion(join(root, "src-tauri/Cargo.toml"), /(^version = ")([^\"]+)(")/m, version);
  replaceVersion(join(root, "src-tauri/Cargo.lock"), /(^name = "toka"\nversion = ")([^\"]+)(")/m, version);

  const gitShaValue = gitSha();
  const builtAt = new Date().toISOString();
  const info = { version, builtAt, gitSha: gitShaValue };
  writeFileSync(join(root, "src-tauri/build-info.json"), `${JSON.stringify(info, null, 2)}\n`);
  return info;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  console.log(JSON.stringify(prepareBuild()));
}
