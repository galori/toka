import { mkdtemp, readdir, stat } from "node:fs/promises";
import os from "node:os";
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

describe("generate", () => {
  it("writes the planned files and refuses a non-empty directory", async () => {
    const out = await mkdtemp(path.join(os.tmpdir(), "toka-gen-"));
    const plan = await generate({ subfolders: 5, out, seed: 2 });
    for (const { file } of plan) {
      expect((await stat(path.join(out, file))).size).toBeGreaterThan(0);
    }
    expect((await readdir(out)).length).toBeGreaterThan(0);
    await expect(generate({ subfolders: 1, out })).rejects.toThrow(/not empty/);
  });
});
