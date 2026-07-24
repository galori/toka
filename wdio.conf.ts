import { delimiter, resolve } from "node:path";
import type { Options } from "@wdio/types";
import { startFixtureServer, stopFixtureServer } from "./test/integration/fixture-server";

const fixturePaths = [1, 2, 3, 4, 5].map((number) =>
  resolve("test/fixtures/sample" + number + ".mp4"),
);
process.env.TOKA_E2E_VIDEOS = fixturePaths.join(delimiter);

const binaryPath = resolve(
  `src-tauri/target/debug/toka${process.platform === "win32" ? ".exe" : ""}`,
);

export const config: Options.Testrunner = {
  runner: "local",
  specs: [
    "./test/integration/happy-path.spec.ts",
    "./test/integration/player-controls.spec.ts",
    "./test/integration/subtitles.spec.ts",
  ],
  maxInstances: 1,
  framework: "mocha",
  reporters: ["spec"],
  services: [["tauri", { appBinaryPath: binaryPath, driverProvider: "embedded" }]],
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
  onComplete: stopFixtureServer,
};
