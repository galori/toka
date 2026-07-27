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

export function prepareBuild() {
  const packagePath = join(root, "package.json");
  const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
  const version = packageJson.version;

  const gitShaValue = gitSha();
  const builtAt = new Date().toISOString();
  const info = { version, builtAt, gitSha: gitShaValue };
  writeFileSync(join(root, "src-tauri/build-info.json"), `${JSON.stringify(info, null, 2)}\n`);
  return info;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  console.log(JSON.stringify(prepareBuild()));
}
