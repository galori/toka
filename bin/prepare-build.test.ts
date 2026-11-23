import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
  prepareBuild,
  resolveBuildVersion,
  resolveBuiltAt,
  resolveIsDirty,
} from "./prepare-build.mjs";

describe("resolveBuiltAt", () => {
  const commitTime = "2026-07-20T09:00:00.000Z";
  const now = "2026-07-27T18:30:00.000Z";

  test("dates a clean tree by its commit so rebuilding it reproduces the same frontend", () => {
    expect(resolveBuiltAt({ isDirty: false, commitTime, now })).toBe(
      commitTime,
    );
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
    "Cargo.lock",
    "src-tauri/Cargo.toml",
    "crates/toka-search/Cargo.toml",
    "src-tauri/tauri.conf.json",
  ];
  const before = new Map(
    trackedFiles.map((path) => [path, readFileSync(path, "utf8")]),
  );

  const info = prepareBuild();

  expect(info.version).toBe(JSON.parse(before.get("package.json")!).version);
  expect(readFileSync(join("src-tauri", "build-info.json"), "utf8")).toContain(
    info.version,
  );
  for (const path of trackedFiles)
    expect(readFileSync(path, "utf8")).toBe(before.get(path));
});

describe("resolveBuildVersion", () => {
  const base = {
    version: "0.1.1",
    commitCount: 482,
    sha: "a1b2c3d4e5f6",
    branch: "main",
    isDirty: false,
  };

  test("appends the commit count and short SHA as semver build metadata", () => {
    expect(resolveBuildVersion(base)).toBe("0.1.1+482.a1b2c3d");
  });

  test("names the branch for worktree builds but not for main", () => {
    expect(resolveBuildVersion({ ...base, branch: "feat/Add Thing" })).toBe(
      "0.1.1+482.a1b2c3d.feat-Add-Thing",
    );
  });

  test("omits the branch when HEAD is detached", () => {
    expect(resolveBuildVersion({ ...base, branch: "HEAD" })).toBe(
      "0.1.1+482.a1b2c3d",
    );
  });

  test("marks uncommitted changes", () => {
    expect(
      resolveBuildVersion({ ...base, branch: "fix-x", isDirty: true }),
    ).toBe("0.1.1+482.a1b2c3d.fix-x.dirty");
  });

  test("truncates long branch names and drops labels with no usable characters", () => {
    expect(resolveBuildVersion({ ...base, branch: "x".repeat(80) })).toBe(
      `0.1.1+482.a1b2c3d.${"x".repeat(32)}`,
    );
    expect(resolveBuildVersion({ ...base, branch: "///" })).toBe(
      "0.1.1+482.a1b2c3d",
    );
  });

  test("falls back to the bare version when git metadata is unavailable", () => {
    expect(resolveBuildVersion({ ...base, commitCount: null, sha: null })).toBe(
      "0.1.1",
    );
  });
});

test("prepareBuild keeps the manifest version and adds a build version", () => {
  const info = prepareBuild();

  expect(info.buildVersion.startsWith(info.version)).toBe(true);
});

describe("resolveIsDirty", () => {
  test("treats a release-stamped checkout as the clean commit it was stamped from", () => {
    expect(
      resolveIsDirty({
        status: " M package.json\n",
        releaseVersion: "0.1.249",
      }),
    ).toBe(false);
  });

  test("reads git status for ordinary builds", () => {
    expect(
      resolveIsDirty({
        status: " M package.json\n",
        releaseVersion: undefined,
      }),
    ).toBe(true);
    expect(resolveIsDirty({ status: "", releaseVersion: undefined })).toBe(
      false,
    );
  });
});

test("marks a release-stamped build clean even when the stamp dirties the tree", () => {
  const previous = process.env.RELEASE_VERSION;
  process.env.RELEASE_VERSION = "0.1.249";
  try {
    expect(prepareBuild().buildVersion).not.toContain("dirty");
  } finally {
    if (previous === undefined) delete process.env.RELEASE_VERSION;
    else process.env.RELEASE_VERSION = previous;
  }
});
