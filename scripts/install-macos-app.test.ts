import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { installMacosApp } from "./install-macos-app.mjs";

const tempRoots: string[] = [];

function makeTempRoot() {
  const root = fs.mkdtempSync(
    path.join(os.tmpdir(), "toka-install-macos-app-"),
  );
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
    const homeDir = path.join(root, "home");
    const destination = path.join(applicationsDir, "Toka.app");
    const commandLink = path.join(homeDir, ".local", "bin", "toka");

    fs.mkdirSync(path.join(source, "Contents", "MacOS"), { recursive: true });
    fs.writeFileSync(
      path.join(source, "Contents", "MacOS", "toka"),
      "fresh build",
    );
    fs.mkdirSync(destination, { recursive: true });
    fs.writeFileSync(path.join(destination, "stale-file"), "old install");

    expect(installMacosApp({ source, applicationsDir, homeDir })).toBe(
      destination,
    );

    expect(fs.existsSync(source)).toBe(false);
    expect(
      fs.readFileSync(
        path.join(destination, "Contents", "MacOS", "toka"),
        "utf8",
      ),
    ).toBe("fresh build");
    expect(fs.readlinkSync(commandLink)).toBe(
      path.join(destination, "Contents", "MacOS", "toka"),
    );
    expect(fs.existsSync(path.join(destination, "stale-file"))).toBe(false);
  });

  test("keeps the command link working when the app is updated", () => {
    const root = makeTempRoot();
    const source = path.join(root, "build", "Toka.app");
    const applicationsDir = path.join(root, "Applications");
    const homeDir = path.join(root, "home");
    const destination = path.join(applicationsDir, "Toka.app");
    const commandLink = path.join(homeDir, ".local", "bin", "toka");
    const installedBinary = path.join(destination, "Contents", "MacOS", "toka");

    fs.mkdirSync(path.dirname(installedBinary), { recursive: true });
    fs.writeFileSync(installedBinary, "old build");
    fs.mkdirSync(path.join(source, "Contents", "MacOS"), { recursive: true });
    fs.writeFileSync(
      path.join(source, "Contents", "MacOS", "toka"),
      "new build",
    );
    fs.mkdirSync(path.dirname(commandLink), { recursive: true });
    fs.symlinkSync(installedBinary, commandLink);

    installMacosApp({ source, applicationsDir, homeDir });

    expect(fs.readlinkSync(commandLink)).toBe(installedBinary);
    expect(fs.readFileSync(commandLink, "utf8")).toBe("new build");
  });

  test("fails clearly when the built app bundle is missing", () => {
    const root = makeTempRoot();

    expect(() =>
      installMacosApp({
        source: path.join(root, "missing", "Toka.app"),
        applicationsDir: path.join(root, "Applications"),
      }),
    ).toThrow(/App bundle not found/);
  });

  test("does not replace an existing shell command", () => {
    const root = makeTempRoot();
    const source = path.join(root, "build", "Toka.app");
    const homeDir = path.join(root, "home");
    const commandLink = path.join(homeDir, ".local", "bin", "toka");

    fs.mkdirSync(path.dirname(commandLink), { recursive: true });
    fs.writeFileSync(commandLink, "user command");
    fs.mkdirSync(path.join(source, "Contents", "MacOS"), { recursive: true });
    fs.writeFileSync(path.join(source, "Contents", "MacOS", "toka"), "new app");

    expect(() => installMacosApp({ source, homeDir })).toThrow(
      /already exists and is not a Toka link/,
    );
    expect(fs.readFileSync(commandLink, "utf8")).toBe("user command");
    expect(fs.existsSync(source)).toBe(true);
  });

  test("does not replace a shell link to another command", () => {
    const root = makeTempRoot();
    const source = path.join(root, "build", "Toka.app");
    const homeDir = path.join(root, "home");
    const commandLink = path.join(homeDir, ".local", "bin", "toka");
    const userCommand = path.join(root, "user-command");

    fs.mkdirSync(path.dirname(commandLink), { recursive: true });
    fs.writeFileSync(userCommand, "user command");
    fs.symlinkSync(userCommand, commandLink);
    fs.mkdirSync(path.join(source, "Contents", "MacOS"), { recursive: true });
    fs.writeFileSync(path.join(source, "Contents", "MacOS", "toka"), "new app");

    expect(() => installMacosApp({ source, homeDir })).toThrow(
      /already points to another target/,
    );
    expect(fs.readlinkSync(commandLink)).toBe(userCommand);
    expect(fs.existsSync(source)).toBe(true);
  });
});
