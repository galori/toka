import { randomInt } from "node:crypto";
import { resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { prepareBuild } from "../bin/prepare-build.mjs";

const task = process.argv[2];
const supportedTasks = new Set(["all", "standard", "owned", "native"]);

if (!supportedTasks.has(task)) {
  throw new Error(
    `Expected one of ${[...supportedTasks].join(", ")}, received ${task ?? "nothing"}.`,
  );
}

if (process.platform !== "linux" && (task === "owned" || task === "native")) {
  process.exit(0);
}

// The frontend is built before WebdriverIO starts the fixture server, so pick
// the port once and pass it to both processes. Random high ports make separate
// worktrees independent without leaving a cross-worktree lock behind.
const fixturePort = String(randomInt(20_000, 60_000));
const cargoTargetDirectory = resolve(
  "src-tauri",
  "target",
  `e2e-${fixturePort}`,
);
// `ts` has no build script, so its version line reads only these two
// variables: without them a dev `ts --version` shows the bare Cargo
// version with `(Git SHA development)` while the app shows the real SHA.
const buildInfo = prepareBuild();
const devGitSha = /^[0-9a-f]+$/i.test(buildInfo.gitSha ?? "")
  ? buildInfo.gitSha
  : undefined;
const environment = {
  ...process.env,
  CARGO_TARGET_DIR: cargoTargetDirectory,
  TOKA_E2E_FIXTURE_SERVER_PORT: fixturePort,
  TOKA_E2E_BINARY: resolve(cargoTargetDirectory, "debug", "toka"),
  TOKA_E2E_FIXTURE_RUN_ID: fixturePort,
  VITE_E2E_FIXTURE_SERVER_PORT: fixturePort,
  TS_BUILD_VERSION: buildInfo.buildVersion,
  ...(devGitSha === undefined ? {} : { TOKA_GIT_SHA: devGitSha }),
};
const wdio = resolve("node_modules/.bin/wdio");

function run(command, args, environmentOverride) {
  const result = spawnSync(command, args, {
    stdio: "inherit",
    env: environmentOverride ?? environment,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function buildE2e() {
  run("npm", ["run", "build:integration"]);
  // The e2e app searches through `ts`: a fixtures build beside the e2e
  // `toka`, resolved as its sibling with the fixture videos forwarded.
  run("cargo", ["build", "-p", "ts", "--features", "fixtures"]);
}

function runWdio(config, environmentOverride) {
  run(wdio, ["run", config], environmentOverride);
}

// The owned-index spec drives the real app and its real `ts` daemon, so
// it needs a non-e2e build in its own target directory.
function buildOwned() {
  const targetDirectory = resolve("src-tauri", "target", `owned-${fixturePort}`);
  const ownedEnvironment = {
    ...environment,
    CARGO_TARGET_DIR: targetDirectory,
    TOKA_E2E_BINARY: resolve(targetDirectory, "debug", "toka"),
  };
  delete ownedEnvironment.VITE_E2E;
  delete ownedEnvironment.VITE_E2E_FIXTURE_SERVER_PORT;
  // `webdriver` registers the embedded WebDriver server without the e2e
  // fixtures; the owned config exposes the `window.__TAURI__` global the
  // spec invokes through, like the e2e config does.
  run(
    "npm",
    [
      "run",
      "tauri",
      "build",
      "--",
      "--debug",
      "--no-bundle",
      "--features",
      "webdriver",
      "--config",
      "src-tauri/tauri.owned.conf.json",
    ],
    ownedEnvironment,
  );
  // The owned app is the real app: it needs the real `ts` beside it —
  // for its own calls and for the daemon the wdio config spawns.
  run("cargo", ["build", "-p", "ts"], ownedEnvironment);
  return ownedEnvironment;
}

if (task === "standard") {
  buildE2e();
  runWdio("wdio.conf.ts");
} else if (task === "owned") {
  const ownedEnvironment = buildOwned();
  runWdio("wdio.owned.conf.ts", ownedEnvironment);
} else if (task === "native") {
  run("npm", ["run", "build:integration:native"]);
  run("cargo", ["build", "-p", "ts", "--features", "fixtures"]);
  runWdio("wdio.native.conf.ts");
} else {
  buildE2e();
  runWdio("wdio.conf.ts");
  if (process.platform === "linux") {
    const ownedEnvironment = buildOwned();
    runWdio("wdio.owned.conf.ts", ownedEnvironment);
  }
  if (process.platform === "linux") {
    run("npm", ["run", "build:integration:native"]);
    run("cargo", ["build", "-p", "ts", "--features", "fixtures"]);
    runWdio("wdio.native.conf.ts");
  }
}
