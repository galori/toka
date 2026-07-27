import { randomInt } from "node:crypto";
import { resolve } from "node:path";
import { spawnSync } from "node:child_process";

const task = process.argv[2];
const supportedTasks = new Set(["all", "standard", "plocate", "native"]);

if (!supportedTasks.has(task)) {
  throw new Error(
    `Expected one of ${[...supportedTasks].join(", ")}, received ${task ?? "nothing"}.`,
  );
}

if (process.platform !== "linux" && (task === "plocate" || task === "native")) {
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
const environment = {
  ...process.env,
  CARGO_TARGET_DIR: cargoTargetDirectory,
  TOKA_E2E_FIXTURE_SERVER_PORT: fixturePort,
  TOKA_E2E_BINARY: resolve(cargoTargetDirectory, "debug", "toka"),
  VITE_E2E_FIXTURE_SERVER_PORT: fixturePort,
};
const wdio = resolve("node_modules/.bin/wdio");

function run(command, args) {
  const result = spawnSync(command, args, {
    stdio: "inherit",
    env: environment,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function buildE2e() {
  run("npm", ["run", "build:integration"]);
}

function runWdio(config) {
  run(wdio, ["run", config]);
}

if (task === "standard") {
  buildE2e();
  runWdio("wdio.conf.ts");
} else if (task === "plocate") {
  buildE2e();
  runWdio("wdio.plocate.conf.ts");
} else if (task === "native") {
  run("npm", ["run", "build:integration:native"]);
  runWdio("wdio.native.conf.ts");
} else {
  buildE2e();
  runWdio("wdio.conf.ts");
  if (process.platform === "linux") runWdio("wdio.plocate.conf.ts");
  if (process.platform === "linux") {
    run("npm", ["run", "build:integration:native"]);
    runWdio("wdio.native.conf.ts");
  }
}
