import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { installMacosApp } from "./install-macos-app.mjs";

const tempRoots: string[] = [];

function makeTempRoot() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "toka-install-macos-app-"));
  tempRoots.push(root);
  return root;
}

afterEach(() => {
  for (const root of tempRoots.splice(0)) {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

describe("installMacosApp", () => {
  test("moves the built app bundle into Applications and replaces an existing install", () => {
    const root = makeTempRoot();
    const source = path.join(root, "build", "Toka.app");
    const applicationsDir = path.join(root, "Applications");
    const destination = path.join(applicationsDir, "Toka.app");

    fs.mkdirSync(path.join(source, "Contents", "MacOS"), { recursive: true });
    fs.writeFileSync(path.join(source, "Contents", "MacOS", "toka"), "fresh build");
    fs.mkdirSync(destination, { recursive: true });
    fs.writeFileSync(path.join(destination, "stale-file"), "old install");

    expect(installMacosApp({ source, applicationsDir })).toBe(destination);

    expect(fs.existsSync(source)).toBe(false);
    expect(fs.readFileSync(path.join(destination, "Contents", "MacOS", "toka"), "utf8")).toBe("fresh build");
    expect(fs.existsSync(path.join(destination, "stale-file"))).toBe(false);
  });

  test("fails clearly when the built app bundle is missing", () => {
    const root = makeTempRoot();

    expect(() => installMacosApp({
      source: path.join(root, "missing", "Toka.app"),
      applicationsDir: path.join(root, "Applications"),
    })).toThrow(/App bundle not found/);
  });
});
