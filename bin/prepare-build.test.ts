import { readFileSync } from "node:fs";
import { join } from "node:path";
import { prepareBuild, resolveBuiltAt } from "./prepare-build.mjs";

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
