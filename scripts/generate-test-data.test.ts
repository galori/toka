import { spawnSync } from "node:child_process";
import { mkdir, mkdtemp, readdir, rm, stat } from "node:fs/promises";
import path from "node:path";
import { generate, makeRng, planTree } from "./generate-test-data.mjs";

describe("planTree", () => {
  it("makes one folder per requested subfolder plus the root, each with one file", () => {
    const plan = planTree(10, makeRng(1));
    expect(plan.folders).toHaveLength(11);
    expect(new Set(plan.folders).size).toBe(11);
  });

  it("is reproducible for a given seed", () => {
    expect(planTree(20, makeRng(7))).toEqual(planTree(20, makeRng(7)));
  });

  it("nests subfolders under existing folders", () => {
    const plan = planTree(50, makeRng(3));
    const folders = new Set(plan.folders);
    for (const folder of plan.folders) {
      expect(
        folders.has(path.dirname(folder) === "." ? "" : path.dirname(folder)),
      ).toBe(true);
    }
    expect(plan.folders.some((f) => f.split(path.sep).length > 1)).toBe(true);
  });
});

const scratch = path.resolve(".scratch");

describe("planTree per-folder counts", () => {
  const count = (
    plan: ReturnType<typeof planTree>,
    folder: string,
    ext: string,
  ) =>
    plan.files.filter((f) => f.folder === folder && f.file.endsWith(ext))
      .length;

  it("never exceeds the maximums and varies between folders", () => {
    const plan = planTree(200, makeRng(5), { maxVideos: 3, maxImages: 2 });
    const videos = plan.folders.map((f) => count(plan, f, ".mp4"));
    const images = plan.folders.map((f) => count(plan, f, ".png"));
    expect(Math.max(...videos)).toBe(3);
    expect(Math.min(...videos)).toBe(0);
    expect(Math.max(...images)).toBe(2);
    expect(Math.min(...images)).toBe(0);
  });

  it("produces no files of a kind whose maximum is 0", () => {
    const plan = planTree(30, makeRng(9), { maxVideos: 0, maxImages: 4 });
    expect(plan.files.some((f) => f.file.endsWith(".mp4"))).toBe(false);
    expect(plan.files.some((f) => f.file.endsWith(".png"))).toBe(true);
  });
});

describe("generate", () => {
  let out: string | undefined;
  afterEach(async () => {
    // Only this test's own output: `.scratch` is shared with other test
    // files running in parallel, so cleaning all of it would race them.
    if (out !== undefined) {
      await rm(out, { recursive: true, force: true });
      out = undefined;
    }
  });

  it("writes the planned files and refuses a non-empty directory", async () => {
    await mkdir(scratch, { recursive: true });
    out = await mkdtemp(path.join(scratch, "gen-"));
    const plan = await generate({ subfolders: 5, out, seed: 2 });
    for (const { file } of plan.files) {
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

  it("rejects an invalid per-folder maximum", () => {
    const r = spawnSync("node", [script, "3", "--max-videos", "-1"], {
      encoding: "utf8",
    });
    expect(r.status).toBe(1);
    expect(r.stderr).toMatch(/--max-videos must be an integer/);
  });

  it("rejects a non-numeric count", () => {
    expect(spawnSync("node", [script, "abc"]).status).toBe(1);
  });
});
