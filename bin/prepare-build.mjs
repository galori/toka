import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function git(args) {
  const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
  if (result.status !== 0) throw result.error ?? new Error(`git ${args[0]} failed`);
  return result.stdout.trim();
}

// A wall-clock build time would differ on every run, and the frontend bundle
// embeds it. That rewrites `dist`, which invalidates `generate_context!` and
// forces a full release rebuild of the Rust crate even when nothing changed.
// Dating a clean tree by its commit keeps repeat builds of the same source
// reproducible; uncommitted work has no commit time, so it falls back to now.
export function resolveBuiltAt({ isDirty, commitTime, now }) {
  return isDirty ? now : commitTime;
}

export function prepareBuild() {
  const packagePath = join(root, "package.json");
  const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
  const version = packageJson.version;

  const gitShaValue = git(["rev-parse", "HEAD"]);
  const builtAt = resolveBuiltAt({
    isDirty: git(["status", "--porcelain"]) !== "",
    commitTime: new Date(git(["show", "-s", "--format=%cI", "HEAD"])).toISOString(),
    now: new Date().toISOString(),
  });
  const info = { version, builtAt, gitSha: gitShaValue };
  writeFileSync(join(root, "src-tauri/build-info.json"), `${JSON.stringify(info, null, 2)}\n`);
  return info;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  console.log(JSON.stringify(prepareBuild()));
}
