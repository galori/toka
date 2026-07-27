import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Options } from "@wdio/types";

process.env.TOKA_E2E_VIDEOS = resolve("test/fixtures/native-blue.mp4");
const temporaryDirectory = mkdtempSync(join(tmpdir(), "toka-e2e-native-"));
process.env.TOKA_E2E_MPV_LOG = join(temporaryDirectory, "mpv.log");

const binaryPath = process.env.TOKA_E2E_BINARY ?? resolve("src-tauri/target/debug/toka");

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
  // The software GL stack in CI intermittently brings up a launch that never
  // presents a frame to the readback (tracked in #57): some runs report a
  // healthy blueRenderCount and some report zero on identical code. Retrying
  // the spec gives the flaky launch another attempt while the root cause is
  // investigated, rather than letting it block every unrelated PR.
  mochaOpts: { timeout: 90_000, retries: 2 },
  onComplete: () => rmSync(temporaryDirectory, { recursive: true, force: true }),
};
