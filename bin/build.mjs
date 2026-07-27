import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { prepareBuild } from "./prepare-build.mjs";

const root = join(fileURLToPath(new URL(".", import.meta.url)), "..");
const target = process.argv[2];
if (!["linux", "mac"].includes(target)) throw new Error("Usage: node bin/build.mjs <linux|mac>");

const info = prepareBuild();
const env = {
  ...process.env,
  VITE_APP_VERSION: info.version,
  VITE_BUILD_TIME: info.builtAt,
  VITE_GIT_SHA: info.gitSha,
};
const bundles = target === "linux" ? "appimage" : "app,dmg";
const build = spawnSync("npm", ["run", "tauri", "build", "--", "--bundles", bundles], {
  cwd: root,
  env,
  stdio: "inherit",
});
if (build.status !== 0) process.exit(build.status ?? 1);

if (target === "linux") {
  const install = spawnSync("node", ["scripts/install-linux.mjs"], { cwd: root, stdio: "inherit" });
  process.exit(install.status ?? 1);
}

const install = spawnSync("node", ["scripts/install-macos-app.mjs"], { cwd: root, stdio: "inherit" });
process.exit(install.status ?? 1);
