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

// The manifests hold only the base semver, bumped deliberately for a release.
// Every build gets a unique, coordination-free suffix from git, so parallel PRs
// never fight over a version number: `0.1.1+482.a1b2c3d.my-branch.dirty`.
export function resolveBuildVersion({ version, commitCount, sha, branch, isDirty }) {
  if (commitCount == null || !sha) return version;
  const parts = [String(commitCount), sha.slice(0, 7)];
  const label = (branch ?? "").replace(/[^0-9A-Za-z-]+/g, "-").replace(/^-+|-+$/g, "")
    .slice(0, 32);
  if (label && label !== "main" && label !== "HEAD") parts.push(label);
  if (isDirty) parts.push("dirty");
  return `${version}+${parts.join(".")}`;
}

function tryGit(args) {
  try {
    return git(args);
  } catch {
    return null;
  }
}

export function prepareBuild() {
  const packagePath = join(root, "package.json");
  const packageJson = JSON.parse(readFileSync(packagePath, "utf8"));
  const version = packageJson.version;

  const gitShaValue = git(["rev-parse", "HEAD"]);
  const isDirty = git(["status", "--porcelain"]) !== "";
  const count = tryGit(["rev-list", "--count", "HEAD"]);
  const buildVersion = resolveBuildVersion({
    version,
    commitCount: count === null ? null : Number(count),
    sha: gitShaValue,
    branch: tryGit(["rev-parse", "--abbrev-ref", "HEAD"]),
    isDirty,
  });
  const builtAt = resolveBuiltAt({
    isDirty,
    commitTime: new Date(git(["show", "-s", "--format=%cI", "HEAD"])).toISOString(),
    now: new Date().toISOString(),
  });
  const info = { version, buildVersion, builtAt, gitSha: gitShaValue };
  writeFileSync(join(root, "src-tauri/build-info.json"), `${JSON.stringify(info, null, 2)}\n`);
  return info;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  console.log(JSON.stringify(prepareBuild()));
}
