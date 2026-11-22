import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { prepareBuild } from "./prepare-build.mjs";
import { builtArtifactPath, parseBuildArgs } from "./build-args.mjs";

const root = join(fileURLToPath(new URL(".", import.meta.url)), "..");
const {
  target,
  install,
  appimage: wantsAppImage,
} = parseBuildArgs(process.argv.slice(2));

const info = prepareBuild();
const env = {
  ...process.env,
  VITE_APP_VERSION: info.buildVersion,
  VITE_BUILD_TIME: info.builtAt,
  VITE_GIT_SHA: info.gitSha,
};

function bundleArguments() {
  if (target === "mac") return ["--bundles", "app,dmg"];
  return wantsAppImage ? ["--bundles", "appimage"] : ["--no-bundle"];
}

const build = spawnSync(
  "npm",
  ["run", "tauri", "build", "--", ...bundleArguments()],
  {
    cwd: root,
    env,
    stdio: "inherit",
  },
);
if (build.status !== 0) process.exit(build.status ?? 1);

console.log(
  `Built: ${builtArtifactPath({ target, appimage: wantsAppImage }, root)}`,
);
if (!install) {
  console.log(
    "Not installed (pass --install, or use npm run build:" +
      target +
      ":install).",
  );
  process.exit(0);
}

if (target === "linux") {
  // Installation updates the user's shared application directory rather than
  // this worktree. Keep builds parallel, but serialize that final operation.
  const result = spawnSync(
    "flock",
    [
      "/tmp/toka-linux-app-install.lock",
      "node",
      "scripts/install-linux.mjs",
      ...(wantsAppImage ? ["--appimage"] : []),
    ],
    { cwd: root, stdio: "inherit" },
  );
  process.exit(result.status ?? 1);
}

const result = spawnSync("node", ["scripts/install-macos-app.mjs"], {
  cwd: root,
  stdio: "inherit",
});
process.exit(result.status ?? 1);
