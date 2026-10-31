import { copyFileSync, mkdirSync, rmSync } from "node:fs";
import { delimiter, resolve } from "node:path";
import type { Options } from "@wdio/types";
import {
  startFixtureServer,
  stopFixtureServer,
} from "./test/integration/fixture-server";

const fixtureRunId =
  process.env.TOKA_E2E_FIXTURE_RUN_ID ??
  process.env.TOKA_E2E_FIXTURE_SERVER_PORT ??
  String(process.pid);
process.env.TOKA_E2E_FIXTURE_RUN_ID = fixtureRunId;
const generatedFixtureDirectory = resolve(
  "src-tauri",
  "target",
  `e2e-addon-fixtures-${fixtureRunId}`,
);
mkdirSync(generatedFixtureDirectory, { recursive: true });
const addonFixtureNames = [
  "2026-10-10_@sometiktoker_18938383490812348343.mp4",
  "2026-10-11_@sometiktoker_18938383490812348344.mp4",
  "2026-10-12_@sometiktoker2_18938383490812348345.mp4",
  "unrelated-addon-fixture.mp4",
];
const addonFixturePaths = addonFixtureNames.map((name, index) => {
  const destination = resolve(generatedFixtureDirectory, name);
  copyFileSync(
    resolve("test/fixtures", `sample${(index % 4) + 1}.mp4`),
    destination,
  );
  return destination;
});

const fixturePaths = [
  ...[1, 2, 3, 4, 5].map((number) =>
    resolve("test/fixtures/sample" + number + ".mp4"),
  ),
  ...addonFixturePaths,
  ...["tall", "narrow", "wide", "short"].map((shape) =>
    resolve("test/fixtures/image-fit-" + shape + ".png"),
  ),
];
process.env.TOKA_E2E_VIDEOS = fixturePaths.join(delimiter);

const binaryPath =
  process.env.TOKA_E2E_BINARY ??
  resolve(
    `src-tauri/target/debug/toka${process.platform === "win32" ? ".exe" : ""}`,
  );

export const config: Options.Testrunner = {
  runner: "local",
  specs: [
    "./test/integration/happy-path.spec.ts",
    "./test/integration/player-controls.spec.ts",
    "./test/integration/addons.spec.ts",
    "./test/integration/image-fit.spec.ts",
    "./test/integration/playlist-ui.spec.ts",
    "./test/integration/subtitles.spec.ts",
    "./test/integration/fullscreen.spec.ts",
    "./test/integration/cursor-hiding.spec.ts",
  ],
  maxInstances: 1,
  framework: "mocha",
  reporters: ["spec"],
  services: [
    ["tauri", { appBinaryPath: binaryPath, driverProvider: "embedded" }],
  ],
  capabilities: [
    {
      browserName: "tauri",
      "tauri:options": { application: binaryPath },
    },
  ],
  logLevel: "warn",
  waitforTimeout: 10_000,
  connectionRetryTimeout: 30_000,
  mochaOpts: { timeout: 30_000 },
  onPrepare: () => startFixtureServer(fixturePaths),
  onComplete: async () => {
    await stopFixtureServer();
    rmSync(generatedFixtureDirectory, { recursive: true, force: true });
  },
};
