import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  assertStampAllowed,
  deriveRelease,
  formatEnv,
  stampManifests,
} from "./prepare-release.mjs";

describe("deriveRelease", () => {
  test("builds a monotonic version and tag from the base minor and commit count", () => {
    expect(deriveRelease({ baseVersion: "0.1.1", commitCount: 482 })).toEqual({
      version: "0.1.482",
      tag: "v0.1.482",
    });
  });

  test("ignores the base patch segment, which automation owns", () => {
    expect(deriveRelease({ baseVersion: "0.2.7", commitCount: 5 })).toEqual({
      version: "0.2.5",
      tag: "v0.2.5",
    });
  });

  test("rejects versions and counts it cannot build a release from", () => {
    expect(() =>
      deriveRelease({ baseVersion: "not-a-version", commitCount: 482 }),
    ).toThrow();
    expect(() =>
      deriveRelease({ baseVersion: "0.1.1", commitCount: 0 }),
    ).toThrow();
    expect(() =>
      deriveRelease({ baseVersion: "0.1.1", commitCount: -3 }),
    ).toThrow();
    expect(() =>
      deriveRelease({ baseVersion: "0.1.1", commitCount: 4.5 }),
    ).toThrow();
  });
});

describe("stampManifests", () => {
  const version = "0.1.482";

  function writeFixture(root: string) {
    writeFileSync(
      join(root, "package.json"),
      `${JSON.stringify({ name: "toka", version: "0.1.1" }, null, 2)}\n`,
    );
    writeFileSync(
      join(root, "package-lock.json"),
      `${JSON.stringify(
        {
          name: "toka",
          version: "0.1.1",
          packages: { "": { name: "toka", version: "0.1.1" } },
        },
        null,
        2,
      )}\n`,
    );
    mkdirSync(join(root, "src-tauri"), { recursive: true });
    writeFileSync(
      join(root, "src-tauri", "Cargo.toml"),
      '[package]\nname = "toka"\nversion = "0.1.1"\n\n[dependencies]\nfoo = "1"\n',
    );
    writeFileSync(
      join(root, "src-tauri", "tauri.conf.json"),
      `${JSON.stringify({ productName: "Toka", version: "0.1.1" }, null, 2)}\n`,
    );
    mkdirSync(join(root, "crates", "toka-search"), { recursive: true });
    writeFileSync(
      join(root, "crates", "toka-search", "Cargo.toml"),
      '[package]\nname = "toka-search"\nversion = "0.1.1"\n',
    );
  }

  function freshRoot() {
    const root = mkdtempSync(join(tmpdir(), "prepare-release-"));
    writeFixture(root);
    return root;
  }

  function readJson(root: string, ...parts: string[]) {
    return JSON.parse(readFileSync(join(root, ...parts), "utf8"));
  }

  test("stamps the release version into every bundle manifest", () => {
    const root = freshRoot();

    stampManifests(root, version);

    expect(readJson(root, "package.json").version).toBe(version);
    const lock = readJson(root, "package-lock.json");
    expect(lock.version).toBe(version);
    expect(lock.packages[""].version).toBe(version);
    expect(
      readFileSync(join(root, "src-tauri", "Cargo.toml"), "utf8"),
    ).toContain(`version = "${version}"`);
    expect(readJson(root, "src-tauri", "tauri.conf.json").version).toBe(
      version,
    );
  });

  test("leaves internal crate versions alone", () => {
    const root = freshRoot();

    stampManifests(root, version);

    expect(
      readFileSync(join(root, "crates", "toka-search", "Cargo.toml"), "utf8"),
    ).toContain('version = "0.1.1"');
  });

  test("refuses to stamp a version that is not plain semver", () => {
    expect(() => stampManifests(freshRoot(), "0.1.482+dirty")).toThrow();
    expect(() => stampManifests(freshRoot(), "v0.1.482")).toThrow();
  });

  test("fails loudly when a manifest has no version to stamp", () => {
    const root = freshRoot();
    writeFileSync(
      join(root, "src-tauri", "Cargo.toml"),
      '[package]\nname = "toka"\n',
    );

    expect(() => stampManifests(root, version)).toThrow();
  });
});

describe("release environment", () => {
  test("formats the workflow environment lines", () => {
    expect(formatEnv({ version: "0.1.482", tag: "v0.1.482" })).toBe(
      "RELEASE_VERSION=0.1.482\nRELEASE_TAG=v0.1.482\n",
    );
  });

  test("refuses to stamp a local tree unless forced", () => {
    expect(() => assertStampAllowed({ isCI: false, force: false })).toThrow();
    expect(() =>
      assertStampAllowed({ isCI: true, force: false }),
    ).not.toThrow();
    expect(() =>
      assertStampAllowed({ isCI: false, force: true }),
    ).not.toThrow();
  });
});

describe("release CLI", () => {
  test("prints matching version and tag lines for the current checkout", () => {
    const script = fileURLToPath(
      new URL("./prepare-release.mjs", import.meta.url),
    );
    const output = execFileSync(process.execPath, [script], {
      encoding: "utf8",
    });

    expect(output).toMatch(
      /^RELEASE_VERSION=(\d+\.\d+\.\d+)\nRELEASE_TAG=v\1\n$/,
    );
  });
});
