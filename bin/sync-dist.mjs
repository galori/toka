import { copyFile, mkdir, readFile, readdir, rm, stat } from "node:fs/promises";
import path from "node:path";

async function kindOf(target) {
  try {
    return (await stat(target)).isDirectory() ? "directory" : "file";
  } catch (error) {
    if (error.code === "ENOENT") return "missing";
    throw error;
  }
}

async function entries(dir) {
  try {
    return await readdir(dir, { withFileTypes: true });
  } catch (error) {
    if (error.code === "ENOENT") return [];
    throw error;
  }
}

async function sameContents(left, right) {
  try {
    return (await readFile(left)).equals(await readFile(right));
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}

/**
 * Copies `sourceDir` over `targetDir`, touching only what actually differs.
 *
 * Vite rewrites every file it emits on every run. `tauri-build` watches the
 * frontend directory, so those fresh timestamps invalidate the Rust crate and
 * cost a full optimized rebuild even when the output is byte-identical. Leaving
 * unchanged files exactly as they are keeps that rebuild for the builds that
 * genuinely need it.
 *
 * Returns whether anything in the target changed.
 */
export async function syncDist({ sourceDir, targetDir }) {
  await mkdir(targetDir, { recursive: true });

  const sourceEntries = await entries(sourceDir);
  const expected = new Set(sourceEntries.map((entry) => entry.name));
  let changed = false;

  for (const entry of await entries(targetDir)) {
    if (expected.has(entry.name)) continue;
    await rm(path.join(targetDir, entry.name), { recursive: true, force: true });
    changed = true;
  }

  for (const entry of sourceEntries) {
    const source = path.join(sourceDir, entry.name);
    const target = path.join(targetDir, entry.name);

    // A name can change kind between builds, and neither mkdir nor copyFile
    // will write over the other kind. Clear the target when that happens.
    const existing = await kindOf(target);

    if (entry.isDirectory()) {
      if (existing === "file") {
        await rm(target, { force: true });
        changed = true;
      }
      if (await syncDist({ sourceDir: source, targetDir: target })) changed = true;
      continue;
    }

    if (existing === "file" && (await sameContents(source, target))) continue;
    await rm(target, { recursive: true, force: true });
    await copyFile(source, target);
    changed = true;
  }

  return changed;
}
