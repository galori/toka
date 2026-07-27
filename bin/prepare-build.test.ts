import { readFileSync } from "node:fs";
import { join } from "node:path";
import { prepareBuild } from "./prepare-build.mjs";

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
