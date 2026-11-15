import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import { builtArtifactPath, parseBuildArgs } from "./build-args.mjs";

describe("parseBuildArgs", () => {
  it("does not install by default", () => {
    expect(parseBuildArgs(["linux"])).toEqual({
      target: "linux",
      install: false,
      appimage: false,
    });
    expect(parseBuildArgs(["mac"])).toEqual({
      target: "mac",
      install: false,
      appimage: false,
    });
  });

  it("installs only when --install is given", () => {
    expect(parseBuildArgs(["linux", "--install"]).install).toBe(true);
    expect(parseBuildArgs(["mac", "--install"]).install).toBe(true);
  });

  it("combines --install with --appimage in either order", () => {
    const expected = { target: "linux", install: true, appimage: true };
    expect(parseBuildArgs(["linux", "--install", "--appimage"])).toEqual(
      expected,
    );
    expect(parseBuildArgs(["linux", "--appimage", "--install"])).toEqual(
      expected,
    );
  });

  it("rejects bad targets, unknown flags and --appimage on mac", () => {
    expect(() => parseBuildArgs([])).toThrow("Usage");
    expect(() => parseBuildArgs(["windows"])).toThrow("Usage");
    expect(() => parseBuildArgs(["linux", "--bogus"])).toThrow("--bogus");
    expect(() => parseBuildArgs(["mac", "--appimage"])).toThrow("Linux-only");
  });
});

describe("builtArtifactPath", () => {
  it("points at the worktree's build output", () => {
    expect(
      builtArtifactPath({ target: "linux", appimage: false }, "/w", {}),
    ).toBe("/w/src-tauri/target/release/toka");
    expect(
      builtArtifactPath({ target: "linux", appimage: true }, "/w", {}),
    ).toBe("/w/src-tauri/target/release/bundle/appimage");
    expect(
      builtArtifactPath({ target: "mac", appimage: false }, "/w", {}),
    ).toBe("/w/src-tauri/target/release/bundle/macos/Toka.app");
  });

  it("resolves a relative CARGO_TARGET_DIR against the build root", () => {
    expect(
      builtArtifactPath({ target: "linux", appimage: false }, "/w", {
        CARGO_TARGET_DIR: "tgt",
      }),
    ).toBe("/w/tgt/release/toka");
  });

  it("honours CARGO_TARGET_DIR", () => {
    expect(
      builtArtifactPath({ target: "linux", appimage: false }, "/w", {
        CARGO_TARGET_DIR: "/t",
      }),
    ).toBe("/t/release/toka");
  });
});

describe("build.mjs install guard", () => {
  it("exits before any install step unless --install was given", async () => {
    const source = await readFile("bin/build.mjs", "utf8");
    const guard = source.indexOf("if (!install)");
    expect(guard).toBeGreaterThan(-1);
    expect(source.indexOf("install-linux.mjs")).toBeGreaterThan(guard);
    expect(source.indexOf("install-macos-app.mjs")).toBeGreaterThan(guard);
  });
});
