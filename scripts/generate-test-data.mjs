#!/usr/bin/env node
// Generates a random folder tree filled with copies of a small video or image.
//
//   node scripts/generate-test-data.mjs <subfolders> [--out DIR] [--seed N]
//                                       [--max-videos N] [--max-images N]
//
// Every folder (the root plus each generated subfolder) gets a random number
// of videos (0..max-videos, default 1) and images (0..max-images, default 1). Each subfolder
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
export const MAX_PER_FOLDER = 1000;
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

/**
 * Plans the tree: relative folder paths ("" is the root) and the files in
 * them. Each folder gets a random 0..maxVideos videos and 0..maxImages images.
 */
export function planTree(
  subfolders,
  rng = Math.random,
  { maxVideos = 1, maxImages = 1 } = {},
) {
  const folders = [""];
  for (let i = 1; i <= subfolders; i++) {
    const parent = folders[Math.floor(rng() * folders.length)];
    folders.push(path.join(parent, `folder-${String(i).padStart(4, "0")}`));
  }
  const files = [];
  const pick = (max) => Math.floor(rng() * (max + 1));
  folders.forEach((folder, index) => {
    const tag = String(index).padStart(4, "0");
    for (const [kind, ext, max, source] of [
      ["video", ".mp4", maxVideos, SAMPLE_VIDEO],
      ["image", ".png", maxImages, SAMPLE_IMAGE],
    ]) {
      const n = pick(max);
      for (let k = 1; k <= n; k++) {
        files.push({
          folder,
          file: path.join(folder, `${kind}-${tag}-${k}${ext}`),
          source,
        });
      }
    }
  });
  return { folders, files };
}

export async function generate({
  subfolders,
  out = DEFAULT_OUT,
  seed,
  maxVideos = 1,
  maxImages = 1,
}) {
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
  const plan = planTree(subfolders, rng, { maxVideos, maxImages });
  for (const folder of plan.folders) {
    await mkdir(path.join(out, folder), { recursive: true });
  }
  for (const { file, source } of plan.files) {
    await copyFile(source, path.join(out, file));
  }
  return plan;
}

const VALUE_FLAGS = ["--out", "--seed", "--max-videos", "--max-images"];

function parseArgs(argv) {
  const opts = { out: DEFAULT_OUT };
  let count;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (VALUE_FLAGS.includes(arg)) {
      const value = argv[++i];
      if (value === undefined || value.startsWith("--")) {
        throw new Error(`${arg} needs a value`);
      }
      if (arg === "--out") opts.out = path.resolve(value);
      else if (arg === "--seed") opts.seed = Number(value);
      else {
        const n = Number(value);
        if (!Number.isInteger(n) || n < 0 || n > MAX_PER_FOLDER) {
          throw new Error(
            `${arg} must be an integer from 0 to ${MAX_PER_FOLDER}`,
          );
        }
        opts[arg === "--max-videos" ? "maxVideos" : "maxImages"] = n;
      }
    } else if (count === undefined) count = Number(arg);
    else throw new Error(`Unexpected argument: ${arg}`);
  }
  if (!Number.isInteger(count) || count < 0 || count > MAX_SUBFOLDERS) {
    throw new Error(
      "Usage: generate-test-data.mjs <subfolders> [--out DIR] [--seed N] [--max-videos N] [--max-images N]",
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
      `Created ${plan.folders.length} folders with ${plan.files.length} files in ${opts.out}`,
    );
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
}
