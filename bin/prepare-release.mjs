// Every green main commit is a release: the version is derived deterministically
// from git, so parallel merges never fight over a version number and re-running
// a release rebuilds the same version. The manifests carry only the base
// major.minor; automation owns the patch segment (the main-branch commit
// count), and the tag is the version prefixed with `v`.
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
export function deriveRelease({ baseVersion, commitCount }) {
  const match = /^(\d+)\.(\d+)\.\d+$/.exec(baseVersion ?? "");
  if (!match) {
    throw new Error(
      `Cannot derive a release version from base version ${JSON.stringify(baseVersion)}`,
    );
  }
  if (!Number.isInteger(commitCount) || commitCount <= 0) {
    throw new Error(
      `Cannot derive a release version from commit count ${JSON.stringify(commitCount)}`,
    );
  }
  const version = `${match[1]}.${match[2]}.${commitCount}`;
  return { version, tag: `v${version}` };
}

const PLAIN_SEMVER = /^\d+\.\d+\.\d+$/;

function stampJsonVersion(path, version) {
  const data = JSON.parse(readFileSync(path, "utf8"));
  if (typeof data.version !== "string") {
    throw new Error(`Cannot stamp ${path}: it has no "version" field`);
  }
  data.version = version;
  writeFileSync(path, `${JSON.stringify(data, null, 2)}\n`);
}

function stampPackageLock(path, version) {
  const data = JSON.parse(readFileSync(path, "utf8"));
  if (typeof data?.packages?.[""]?.version !== "string") {
    throw new Error(
      `Cannot stamp ${path}: it has no root packages[""].version`,
    );
  }
  data.version = version;
  data.packages[""].version = version;
  writeFileSync(path, `${JSON.stringify(data, null, 2)}\n`);
}

function stampCargoToml(path, version) {
  const text = readFileSync(path, "utf8");
  const stamped = text.replace(/^version = "[^"]*"/m, `version = "${version}"`);
  if (stamped === text) {
    throw new Error(`Cannot stamp ${path}: it has no package version line`);
  }
  writeFileSync(path, stamped);
}

// Stamps the release version into the manifests the bundles are built from.
// Internal crates keep their own versions: nothing ships them separately.
// Runs only in the release workflow's throwaway checkout, never in a commit.
export function stampManifests(root, version) {
  if (!PLAIN_SEMVER.test(version ?? "")) {
    throw new Error(
      `Cannot stamp manifests with non-semver version ${JSON.stringify(version)}`,
    );
  }
  stampJsonVersion(join(root, "package.json"), version);
  stampPackageLock(join(root, "package-lock.json"), version);
  stampCargoToml(join(root, "src-tauri", "Cargo.toml"), version);
  stampJsonVersion(join(root, "src-tauri", "tauri.conf.json"), version);
}

export function formatEnv({ version, tag }) {
  return `RELEASE_VERSION=${version}\nRELEASE_TAG=${tag}\n`;
}

export function assertStampAllowed({ isCI, force }) {
  if (!isCI && !force) {
    throw new Error(
      "Refusing to stamp manifests outside CI: pass --force to stamp a local tree",
    );
  }
}

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function gitCommitCount() {
  const result = spawnSync("git", ["rev-list", "--count", "HEAD"], {
    cwd: root,
    encoding: "utf8",
  });
  if (result.status !== 0) {
    throw new Error(
      "Cannot derive a release: git rev-list --count HEAD failed",
    );
  }
  return Number(result.stdout.trim());
}

function readBaseVersion() {
  return JSON.parse(readFileSync(join(root, "package.json"), "utf8")).version;
}

const entrypoint = process.argv[1] ? pathToFileURL(process.argv[1]).href : null;
if (import.meta.url === entrypoint) {
  const args = process.argv.slice(2);
  const unknown = args.filter((arg) => arg !== "--stamp" && arg !== "--force");
  if (unknown.length > 0) {
    throw new Error(`Unknown arguments: ${unknown.join(" ")}`);
  }
  const stamp = args.includes("--stamp");
  if (stamp) {
    assertStampAllowed({
      isCI: process.env.CI === "true",
      force: args.includes("--force"),
    });
  }
  const release = deriveRelease({
    baseVersion: readBaseVersion(),
    commitCount: gitCommitCount(),
  });
  if (stamp) stampManifests(root, release.version);
  process.stdout.write(formatEnv(release));
}
