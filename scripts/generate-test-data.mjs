#!/usr/bin/env node
// Generates a random folder tree filled with copies of a small video or image.
//
//   node scripts/generate-test-data.mjs <subfolders> [--out DIR] [--seed N]
//
// Every folder (the root plus each generated subfolder) gets exactly one file,
// randomly a copy of the sample video or of the sample image. Each subfolder
// is attached to a randomly chosen existing folder, so the tree shape varies.
import { access, copyFile, mkdir, readdir } from "node:fs/promises";
import { realpathSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
export const SAMPLE_VIDEO = path.join(repoRoot, "test/fixtures/sample5.mp4");
export const SAMPLE_IMAGE = path.join(
  repoRoot,
  "test/fixtures/Screenshot From 2026-07-27 21-56-52.png",
);
export const MAX_SUBFOLDERS = 100000;
export const DEFAULT_OUT = path.join(repoRoot, ".scratch/test-data");

// mulberry32: tiny seedable PRNG so a tree can be reproduced with --seed.
export function makeRng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Plans the tree: relative folder paths ("" is the root) and the file in each. */
export function planTree(subfolders, rng = Math.random) {
  const folders = [""];
  for (let i = 1; i <= subfolders; i++) {
    const parent = folders[Math.floor(rng() * folders.length)];
    folders.push(path.join(parent, `folder-${String(i).padStart(4, "0")}`));
  }
  return folders.map((folder, index) => {
    const isVideo = rng() < 0.5;
    const name = `${isVideo ? "video" : "image"}-${String(index).padStart(4, "0")}`;
    return {
      folder,
      file: path.join(folder, name + (isVideo ? ".mp4" : ".png")),
      source: isVideo ? SAMPLE_VIDEO : SAMPLE_IMAGE,
    };
  });
}

export async function generate({ subfolders, out = DEFAULT_OUT, seed }) {
  for (const source of [SAMPLE_VIDEO, SAMPLE_IMAGE]) {
    await access(source).catch(() => {
      throw new Error(`Sample file is missing: ${source}`);
    });
  }
  const existing = await readdir(out).catch((error) => {
    if (error.code === "ENOENT") return [];
    throw error;
  });
  if (existing.length > 0) {
    throw new Error(
      `Output directory ${out} is not empty; pick another with --out.`,
    );
  }
  const rng = seed === undefined ? Math.random : makeRng(seed);
  const plan = planTree(subfolders, rng);
  for (const { folder, file, source } of plan) {
    await mkdir(path.join(out, folder), { recursive: true });
    await copyFile(source, path.join(out, file));
  }
  return plan;
}

function parseArgs(argv) {
  const opts = { out: DEFAULT_OUT };
  let count;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--out" || arg === "--seed") {
      const value = argv[++i];
      if (value === undefined || value.startsWith("--")) {
        throw new Error(`${arg} needs a value`);
      }
      if (arg === "--out") opts.out = path.resolve(value);
      else opts.seed = Number(value);
    } else if (count === undefined) count = Number(arg);
    else throw new Error(`Unexpected argument: ${arg}`);
  }
  if (!Number.isInteger(count) || count < 0 || count > MAX_SUBFOLDERS) {
    throw new Error(
      "Usage: generate-test-data.mjs <subfolders> [--out DIR] [--seed N]",
    );
  }
  if (opts.seed !== undefined && !Number.isFinite(opts.seed)) {
    throw new Error("--seed must be a number");
  }
  return { ...opts, subfolders: count };
}

if (
  process.argv[1] &&
  realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))
) {
  try {
    const opts = parseArgs(process.argv.slice(2));
    const plan = await generate(opts);
    console.log(
      `Created ${plan.length} folders with ${plan.length} files in ${opts.out}`,
    );
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
}
