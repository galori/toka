import { spawnSync } from "node:child_process";
import { rm } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { syncDist } from "./sync-dist.mjs";

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const stagingDir = path.join(root, "node_modules", ".cache", "toka-frontend");
const distDir = path.join(root, "dist");

// Vite always empties and rewrites its output directory. Building into a
// staging directory and then syncing only the differences keeps `dist`
// timestamps stable, so an unchanged frontend no longer triggers a full
// release rebuild of the Rust crate. See OPTIMIZE_BUILD_SPEED.md.
await rm(stagingDir, { recursive: true, force: true });

const build = spawnSync("npx", ["vite", "build", "--outDir", stagingDir, "--emptyOutDir"], {
  cwd: root,
  stdio: "inherit",
});
if (build.status !== 0) process.exit(build.status ?? 1);

const changed = await syncDist({ sourceDir: stagingDir, targetDir: distDir });
console.log(changed ? "Frontend output changed." : "Frontend output unchanged; left dist/ untouched.");
