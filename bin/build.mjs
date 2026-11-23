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

function bundlesToPackage() {
  if (target === "mac") return ["app", "dmg"];
  return wantsAppImage ? ["appimage"] : [];
}

// The search helper the app shells out to, built before the app so the
// bundler picks it up as the `binaries/ts` sidecar — and so the Linux
// installer finds the release binary beside the app build. One build
// serves both: the sidecar stage step builds release `ts` itself.
const sidecar = spawnSync("node", ["bin/build-ts-sidecar.mjs"], {
  cwd: root,
  stdio: "inherit",
});
if (sidecar.status !== 0) process.exit(sidecar.status ?? 1);

// Compile first with the base config, then bundle with the sidecar overlay
// merged in — never `tauri build --bundles` with the overlay. The overlay
// reaches the compile's build script through TAURI_CONFIG, and tauri-build
// copies sidecars itself there: it derives the destination as three
// directories above OUT_DIR, which was the profile directory under cargo's
// flat build layout but is `build/` under the nested per-package layout —
// so the copy lands on cargo's own `build/ts` directory and fails. The
// bundler reads the staged sidecar straight from `src-tauri/binaries`,
// which is layout-independent, so only it gets the overlay.
const build = spawnSync("npm", ["run", "tauri", "build", "--", "--no-bundle"], {
  cwd: root,
  env,
  stdio: "inherit",
});
if (build.status !== 0) process.exit(build.status ?? 1);

const bundles = bundlesToPackage();
if (bundles.length > 0) {
  const bundle = spawnSync(
    "npm",
    [
      "run",
      "tauri",
      "bundle",
      "--",
      "--bundles",
      bundles.join(","),
      "--config",
      "src-tauri/tauri.bundle.conf.json",
    ],
    { cwd: root, stdio: "inherit" },
  );
  if (bundle.status !== 0) process.exit(bundle.status ?? 1);
}

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
