import { spawnSync } from "node:child_process";
import { chmod, copyFile, mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { prepareBuild } from "./prepare-build.mjs";

export const USAGE = "Usage: node bin/build-ts-sidecar.mjs [--target <triple>]";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

export function parseSidecarArgs(argv) {
  const [flag, value, ...rest] = argv;
  if (flag === undefined) return { target: undefined };
  if (flag !== "--target" || value === undefined || rest.length > 0) {
    throw new Error(USAGE);
  }
  return { target: value };
}

function probeHostTriple(exec) {
  const result = exec("rustc", ["--print", "host-tuple"]);
  if (result.error !== undefined || result.status !== 0) {
    throw new Error(
      "Could not determine the target triple: rustc --print host-tuple failed. " +
        "Pass --target <triple> explicitly.",
    );
  }
  return result.stdout.trim();
}

// The triple suffix Tauri wants on the staged file: an explicit --target
// passes through untouched, otherwise rustc names the host. `exec` is
// spawnSync in production, a double in tests.
export function resolveTriple(
  { target },
  exec = (command, args) => spawnSync(command, args, { encoding: "utf8" }),
) {
  if (target !== undefined) return target;
  return probeHostTriple(exec);
}

// Where the ts release build lands: the workspace target directory
// (`.cargo/config.toml` points it at `src-tauri/target`, overridable via
// `CARGO_TARGET_DIR`), with a `--target` triple resolving its own
// subdirectory — the same directory the bundler and installers read.
export function tsReleasePath({ cargoTarget }, scriptRoot, env = process.env) {
  const targetDir =
    env.CARGO_TARGET_DIR ?? join(scriptRoot, "src-tauri", "target");
  return join(
    targetDir,
    ...(cargoTarget === undefined ? [] : [cargoTarget]),
    "release",
    "ts",
  );
}

// Where Tauri looks for the sidecar: `externalBin: ["binaries/ts"]` in
// `tauri.conf.json` resolves `src-tauri/binaries/ts-<triple>`, relative
// to the config file. The directory is git-ignored build output.
export function sidecarStagePath(triple, scriptRoot) {
  return join(scriptRoot, "src-tauri", "binaries", `ts-${triple}`);
}

// Builds the release `ts` the bundles ship and stages it for Tauri.
// Version provenance mirrors build.rs: only a hex SHA reaches the binary.
export async function buildTsSidecar({
  triple,
  cargoTarget,
  buildVersion,
  gitSha,
  root: scriptRoot,
  env = process.env,
  spawn = spawnSync,
}) {
  const sha = /^[0-9a-f]+$/i.test(gitSha ?? "") ? gitSha : undefined;
  const build = spawn(
    "cargo",
    [
      "build",
      "--release",
      "-p",
      "ts",
      ...(cargoTarget === undefined ? [] : ["--target", cargoTarget]),
    ],
    {
      cwd: scriptRoot,
      env: {
        ...env,
        TS_BUILD_VERSION: buildVersion,
        ...(sha === undefined ? {} : { TOKA_GIT_SHA: sha }),
      },
      stdio: "inherit",
    },
  );
  if (build.error !== undefined || build.status !== 0) {
    throw new Error(
      `cargo build -p ts failed${build.status ? ` with status ${build.status}` : ""}.`,
    );
  }

  const staged = sidecarStagePath(triple, scriptRoot);
  await mkdir(dirname(staged), { recursive: true });
  await copyFile(tsReleasePath({ cargoTarget }, scriptRoot, env), staged);
  await chmod(staged, 0o755);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const { target } = parseSidecarArgs(process.argv.slice(2));
  const info = prepareBuild();
  await buildTsSidecar({
    triple: resolveTriple({ target }),
    cargoTarget: target,
    buildVersion: info.buildVersion,
    gitSha: info.gitSha,
    root,
  });
}
