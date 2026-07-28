import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { prepareBuild } from "./prepare-build.mjs";

const root = join(fileURLToPath(new URL(".", import.meta.url)), "..");
const target = process.argv[2];
if (!["linux", "mac"].includes(target)) throw new Error("Usage: node bin/build.mjs <linux|mac> [--appimage]");

// AppImage bundling dominates a Linux build and exists to carry GTK and WebKit
// to machines that lack them. A local install runs against the same system
// libraries it was just compiled against, so it installs the executable itself
// and leaves bundling to release builds. See OPTIMIZE_BUILD_SPEED.md.
const wantsAppImage = target === "linux" && process.argv.includes("--appimage");

const info = prepareBuild();
const env = {
  ...process.env,
  VITE_APP_VERSION: info.version,
  VITE_BUILD_TIME: info.builtAt,
  VITE_GIT_SHA: info.gitSha,
};

function bundleArguments() {
  if (target === "mac") return ["--bundles", "app,dmg"];
  return wantsAppImage ? ["--bundles", "appimage"] : ["--no-bundle"];
}

const build = spawnSync("npm", ["run", "tauri", "build", "--", ...bundleArguments()], {
  cwd: root,
  env,
  stdio: "inherit",
});
if (build.status !== 0) process.exit(build.status ?? 1);

if (target === "linux") {
  // Installation updates the user's shared application directory rather than
  // this worktree. Keep builds parallel, but serialize that final operation.
  const install = spawnSync(
    "flock",
    ["/tmp/toka-linux-app-install.lock", "node", "scripts/install-linux.mjs", ...(wantsAppImage ? ["--appimage"] : [])],
    { cwd: root, stdio: "inherit" },
  );
  process.exit(install.status ?? 1);
}

const install = spawnSync("node", ["scripts/install-macos-app.mjs"], { cwd: root, stdio: "inherit" });
process.exit(install.status ?? 1);
