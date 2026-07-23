import { resolve } from "node:path";
import type { Options } from "@wdio/types";

process.env.TOKA_E2E_VIDEOS = resolve("test/fixtures/native-blue.mp4");

const binaryPath = resolve("src-tauri/target/debug/toka");

export const config: Options.Testrunner = {
  runner: "local",
  specs: ["./test/integration/native-playback.spec.ts"],
  maxInstances: 1,
  framework: "mocha",
  reporters: ["spec"],
  services: [["tauri", {
    appBinaryPath: binaryPath,
    driverProvider: "embedded",
  }]],
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
};
