import { mkdir, mkdtemp, readFile, readdir, stat, utimes, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { syncDist } from "./sync-dist.mjs";

async function makeDirs() {
  const root = await mkdtemp(path.join(os.tmpdir(), "toka-sync-dist-"));
  const sourceDir = path.join(root, "staging");
  const targetDir = path.join(root, "dist");
  await mkdir(sourceDir, { recursive: true });
  await mkdir(targetDir, { recursive: true });
  return { sourceDir, targetDir };
}

const ancient = new Date("2020-01-01T00:00:00.000Z");

describe("syncDist", () => {
  it("leaves an unchanged file's timestamp alone so the Rust crate is not invalidated", async () => {
    const { sourceDir, targetDir } = await makeDirs();
    await writeFile(path.join(sourceDir, "index.js"), "same");
    await writeFile(path.join(targetDir, "index.js"), "same");
    await utimes(path.join(targetDir, "index.js"), ancient, ancient);

    const changed = await syncDist({ sourceDir, targetDir });

    const after = await stat(path.join(targetDir, "index.js"));
    expect(after.mtimeMs).toBe(ancient.getTime());
    expect(changed).toBe(false);
  });

  it("replaces a file whose contents changed", async () => {
    const { sourceDir, targetDir } = await makeDirs();
    await writeFile(path.join(sourceDir, "index.js"), "new");
    await writeFile(path.join(targetDir, "index.js"), "old");
    await utimes(path.join(targetDir, "index.js"), ancient, ancient);

    const changed = await syncDist({ sourceDir, targetDir });

    await expect(readFile(path.join(targetDir, "index.js"), "utf8")).resolves.toBe("new");
    const after = await stat(path.join(targetDir, "index.js"));
    expect(after.mtimeMs).toBeGreaterThan(ancient.getTime());
    expect(changed).toBe(true);
  });

  it("copies nested assets that the target does not have yet", async () => {
    const { sourceDir, targetDir } = await makeDirs();
    await mkdir(path.join(sourceDir, "assets"), { recursive: true });
    await writeFile(path.join(sourceDir, "assets", "app.css"), "body{}");

    const changed = await syncDist({ sourceDir, targetDir });

    await expect(readFile(path.join(targetDir, "assets", "app.css"), "utf8")).resolves.toBe("body{}");
    expect(changed).toBe(true);
  });

  it("removes stale hashed bundles that the new build no longer emits", async () => {
    const { sourceDir, targetDir } = await makeDirs();
    await mkdir(path.join(sourceDir, "assets"), { recursive: true });
    await mkdir(path.join(targetDir, "assets"), { recursive: true });
    await writeFile(path.join(sourceDir, "assets", "index-new.js"), "new");
    await writeFile(path.join(targetDir, "assets", "index-old.js"), "old");

    const changed = await syncDist({ sourceDir, targetDir });

    expect(await readdir(path.join(targetDir, "assets"))).toEqual(["index-new.js"]);
    expect(changed).toBe(true);
  });

  it("reports no change when a whole tree is already identical", async () => {
    const { sourceDir, targetDir } = await makeDirs();
    for (const dir of [sourceDir, targetDir]) {
      await mkdir(path.join(dir, "assets"), { recursive: true });
      await writeFile(path.join(dir, "index.html"), "<html></html>");
      await writeFile(path.join(dir, "assets", "app.js"), "console.log(1)");
    }

    expect(await syncDist({ sourceDir, targetDir })).toBe(false);
  });
});
