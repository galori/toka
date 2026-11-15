import { readFileSync } from "node:fs";
import { join } from "node:path";
import { prepareBuild, resolveBuildVersion, resolveBuiltAt } from "./prepare-build.mjs";

describe("resolveBuiltAt", () => {
  const commitTime = "2026-07-20T09:00:00.000Z";
  const now = "2026-07-27T18:30:00.000Z";

  test("dates a clean tree by its commit so rebuilding it reproduces the same frontend", () => {
    expect(resolveBuiltAt({ isDirty: false, commitTime, now })).toBe(commitTime);
  });

  test("dates a dirty tree by wall clock, because uncommitted work has no commit time", () => {
    expect(resolveBuiltAt({ isDirty: true, commitTime, now })).toBe(now);
  });
});

test("packages the generated build provenance file in the Linux deb", () => {
  const config = JSON.parse(readFileSync("src-tauri/tauri.conf.json", "utf8"));

  expect(config.bundle.linux.deb.files).toMatchObject({
    "/usr/share/toka/build-info.json": "build-info.json",
  });
});

test("prepares build provenance without changing tracked version files", () => {
  const trackedFiles = [
    "package.json",
    "package-lock.json",
    "src-tauri/Cargo.toml",
    "src-tauri/Cargo.lock",
    "src-tauri/tauri.conf.json",
  ];
  const before = new Map(trackedFiles.map((path) => [path, readFileSync(path, "utf8")]));

  const info = prepareBuild();

  expect(info.version).toBe(JSON.parse(before.get("package.json")!).version);
  expect(readFileSync(join("src-tauri", "build-info.json"), "utf8")).toContain(info.version);
  for (const path of trackedFiles) expect(readFileSync(path, "utf8")).toBe(before.get(path));
});

describe("resolveBuildVersion", () => {
  const base = { version: "0.1.1", commitCount: 482, sha: "a1b2c3d4e5f6", branch: "main", isDirty: false };

  test("appends the commit count and short SHA as semver build metadata", () => {
    expect(resolveBuildVersion(base)).toBe("0.1.1+482.a1b2c3d");
  });

  test("names the branch for worktree builds but not for main", () => {
    expect(resolveBuildVersion({ ...base, branch: "feat/Add Thing" })).toBe("0.1.1+482.a1b2c3d.feat-Add-Thing");
  });

  test("omits the branch when HEAD is detached", () => {
    expect(resolveBuildVersion({ ...base, branch: "HEAD" })).toBe("0.1.1+482.a1b2c3d");
  });

  test("marks uncommitted changes", () => {
    expect(resolveBuildVersion({ ...base, branch: "fix-x", isDirty: true })).toBe("0.1.1+482.a1b2c3d.fix-x.dirty");
  });

  test("truncates long branch names and drops labels with no usable characters", () => {
    expect(resolveBuildVersion({ ...base, branch: "x".repeat(80) })).toBe(`0.1.1+482.a1b2c3d.${"x".repeat(32)}`);
    expect(resolveBuildVersion({ ...base, branch: "///" })).toBe("0.1.1+482.a1b2c3d");
  });

  test("falls back to the bare version when git metadata is unavailable", () => {
    expect(resolveBuildVersion({ ...base, commitCount: null, sha: null })).toBe("0.1.1");
  });
});

test("prepareBuild keeps the manifest version and adds a build version", () => {
  const info = prepareBuild();

  expect(info.buildVersion.startsWith(info.version)).toBe(true);
});
