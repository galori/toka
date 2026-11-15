import path from "node:path";

export const USAGE =
  "Usage: node bin/build.mjs <linux|mac> [--install] [--appimage]";

// Building never touches the user's installed copy unless --install is given,
// so a PR worktree can be built and run alongside the system Toka.
export function parseBuildArgs(argv) {
  const [target, ...flags] = argv;
  if (!["linux", "mac"].includes(target)) throw new Error(USAGE);
  const unknown = flags.filter(
    (flag) => !["--install", "--appimage"].includes(flag),
  );
  if (unknown.length > 0)
    throw new Error(`Unknown option ${unknown.join(", ")}. ${USAGE}`);
  // AppImage bundling dominates a Linux build and exists to carry GTK and WebKit
  // to machines that lack them. See OPTIMIZE_BUILD_SPEED.md.
  if (flags.includes("--appimage") && target !== "linux")
    throw new Error(`--appimage is Linux-only. ${USAGE}`);
  return {
    target,
    install: flags.includes("--install"),
    appimage: flags.includes("--appimage"),
  };
}

// Where the build left its runnable output, for launching straight from the worktree.
export function builtArtifactPath(
  { target, appimage },
  root,
  env = process.env,
) {
  // Tauri runs Cargo from src-tauri, so a relative CARGO_TARGET_DIR resolves there.
  const releaseDir = path.join(
    path.resolve(root, "src-tauri", env.CARGO_TARGET_DIR ?? "target"),
    "release",
  );
  if (target === "mac")
    return path.join(releaseDir, "bundle", "macos", "Toka.app");
  return appimage
    ? path.join(releaseDir, "bundle", "appimage")
    : path.join(releaseDir, "toka");
}
