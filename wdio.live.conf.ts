import { resolve } from "node:path";
import type { Options } from "@wdio/types";

const binaryPath = resolve(
  `src-tauri/target/debug/toka${process.platform === "win32" ? ".exe" : ""}`,
);

export const config: Options.Testrunner = {
  runner: "local",
  specs: ["./test/integration/live-provider.spec.ts"],
  maxInstances: 1,
  framework: "mocha",
  reporters: ["spec"],
  services: [[
    "tauri",
    {
      appBinaryPath: binaryPath,
      driverProvider: "embedded",
      captureBackendLogs: true,
    },
  ]],
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
