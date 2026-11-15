import { spawnSync } from "node:child_process";
import { mkdir, mkdtemp, readdir, rm, stat } from "node:fs/promises";
import path from "node:path";
import { generate, makeRng, planTree } from "./generate-test-data.mjs";

describe("planTree", () => {
  it("makes one folder per requested subfolder plus the root, each with one file", () => {
    const plan = planTree(10, makeRng(1));
    expect(plan).toHaveLength(11);
    expect(new Set(plan.map((p) => p.folder)).size).toBe(11);
  });

  it("is reproducible for a given seed", () => {
    expect(planTree(20, makeRng(7))).toEqual(planTree(20, makeRng(7)));
  });

  it("nests subfolders under existing folders", () => {
    const plan = planTree(50, makeRng(3));
    const folders = new Set(plan.map((p) => p.folder));
    for (const { folder } of plan) {
      expect(
        folders.has(path.dirname(folder) === "." ? "" : path.dirname(folder)),
      ).toBe(true);
    }
    expect(plan.some((p) => p.folder.split(path.sep).length > 1)).toBe(true);
  });
});

const scratch = path.resolve(".scratch");

describe("generate", () => {
  afterEach(async () => {
    await rm(scratch, { recursive: true, force: true });
  });

  it("writes the planned files and refuses a non-empty directory", async () => {
    await mkdir(scratch, { recursive: true });
    const out = await mkdtemp(path.join(scratch, "gen-"));
    const plan = await generate({ subfolders: 5, out, seed: 2 });
    for (const { file } of plan) {
      expect((await stat(path.join(out, file))).size).toBeGreaterThan(0);
    }
    expect((await readdir(out)).length).toBeGreaterThan(0);
    await expect(generate({ subfolders: 1, out })).rejects.toThrow(/not empty/);
  });
});

describe("cli", () => {
  const script = path.resolve("scripts/generate-test-data.mjs");

  it("rejects a flag without a value instead of using the cwd", () => {
    const r = spawnSync("node", [script, "3", "--out"], { encoding: "utf8" });
    expect(r.status).toBe(1);
    expect(r.stderr).toMatch(/needs a value/);
  });

  it("rejects a non-numeric count", () => {
    expect(spawnSync("node", [script, "abc"]).status).toBe(1);
  });
});
