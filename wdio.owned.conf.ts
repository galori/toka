import {
  chmodSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
  mkdirSync,
} from "node:fs";
import { spawn, type ChildProcess } from "node:child_process";
import { delimiter, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import type { Options } from "@wdio/types";

// A fake home directory, so the app and its indexer daemon only ever touch
// this test's files. XDG variables are set explicitly because the test runner
// may already export them. WebdriverIO re-evaluates this file in each worker,
// so the launcher publishes the home for workers to reuse; otherwise each
// worker would mint its own home and the launcher's cleanup would orphan it.
const homeDirectory =
  process.env.TOKA_E2E_OWNED_HOME ??
  mkdtempSync(join(tmpdir(), "toka-e2e-owned-"));
process.env.TOKA_E2E_OWNED_HOME = homeDirectory;
process.env.HOME = homeDirectory;
process.env.XDG_CONFIG_HOME = join(homeDirectory, ".config");
process.env.XDG_DATA_HOME = join(homeDirectory, ".local", "share");
process.env.XDG_CACHE_HOME = join(homeDirectory, ".cache");

// The owned build is the real app, so it runs `systemctl --user start
// toka-indexer.service` on launch. On a machine with Toka installed that would
// start the real user service against the real index, while this run spawns
// its own daemon below. Shadow systemctl on PATH so the app under test cannot
// reach the installed service; every call succeeds without doing anything,
// which also truthfully reports the test daemon as running.
const shimDirectory = join(homeDirectory, "bin");
mkdirSync(shimDirectory, { recursive: true });
const shimPath = join(shimDirectory, "systemctl");
const shimLog = join(homeDirectory, "systemctl-shim.log");
writeFileSync(
  shimPath,
  `#!/bin/sh\nprintf '%s systemctl %s\\n' "$(date -Is)" "$*" >> '${shimLog.replaceAll("'", "'\\''")}'\nexit 0\n`,
);
chmodSync(shimPath, 0o755);
process.env.PATH = `${shimDirectory}${delimiter}${process.env.PATH ?? ""}`;

const videosDirectory = join(homeDirectory, "Videos");
mkdirSync(videosDirectory, { recursive: true });
const fixturePath = join(videosDirectory, "toka-e2e-happy-path.mp4");
process.env.TOKA_E2E_OWNED_VIDEOS = videosDirectory;
writeFileSync(
  fixturePath,
  Buffer.from(
    "AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXZjMW1wNDEAAAPXbW9vdgAAAGxtdmhkAAAAAAAAAAAAAAAAAAAD6AAAAggAAQAAAQAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAwF0cmFrAAAAXHRraGQAAAADAAAAAAAAAAAAAAABAAAAAAAAAggAAAAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAKAAAABaAAAAAAAkZWR0cwAAABxlbHN0AAAAAAAAAAEAAAIIAAAEAAABAAAAAAJ5bWRpYQAAACBtZGhkAAAAAAAAAAAAAAAAAAAyAAAAGgBVxAAAAAAALWhkbHIAAAAAAAAAAHZpZGUAAAAAAAAAAAAAAABWaWRlb0hhbmRsZXIAAAACJG1pbmYAAAAUdm1oZAAAAAEAAAAAAAAAAAAAACRkaW5mAAAAHGRyZWYAAAAAAAAAAQAAAAx1cmwgAAAAAQAAAeRzdGJsAAAAwHN0c2QAAAAAAAAAAQAAALBhdmMxAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAKAAWgBIAAAASAAAAAAAAAABFUxhdmM2Mi4yOC4xMDIgbGlieDI2NAAAAAAAAAAAAAAAGP//AAAANmF2Y0MBZAAL/+EAGWdkAAus2UKN+TARAAADAAEAAAMAMg8UKZYBAAZo6+PLIsD9+PgAAAAAEHBhc3AAAAABAAAAAQAAABRidHJ0AAAAAAAANs4AAAAAAAAAGHN0dHMAAAAAAAAAAQAAAA0AAAIAAAAAFHN0c3MAAAAAAAAAAQAAAAEAAAB4Y3R0cwAAAAAAAAANAAAAAQAABAAAAAABAAAKAAAAAAEAAAQAAAAAAQAAAAAAAAABAAACAAAAAAEAAAoAAAAAAQAABAAAAAABAAAAAAAAAAEAAAIAAAAAAQAACgAAAAABAAAEAAAAAAEAAAAAAAAAAQAAAgAAAAAcc3RzYwAAAAAAAAABAAAAAQAAAA0AAAABAAAASHN0c3oAAAAAAAAAAAAAAA0AAALkAAAADwAAAA0AAAAMAAAADAAAABUAAAAPAAAADAAAAAwAAAAVAAAADwAAAAwAAAAMAAAAFHN0Y28AAAAAAAAAAQAABAcAAABidWR0YQAAAFptZXRhAAAAAAAAACFoZGxyAAAAAAAAAABtZGlyYXBwbAAAAAAAAAAAAAAAAC1pbHN0AAAAJal0b28AAAAdZGF0YQAAAAEAAAAATGF2ZjYyLjEyLjEwMgAAAAhmcmVlAAADmG1kYXQAAAKuBgX//6rcRem95tlIt5Ys2CDZI+7veDI2NCAtIGNvcmUgMTY1IHIzMjIyIGIzNTYwNWEgLSBILjI2NC9NUEVHLTQgQVZDIGNvZGVjIC0gQ29weWxlZnQgMjAwMy0yMDI1IC0gaHR0cDovL3d3dy52aWRlb2xhbi5vcmcveDI2NC5odG1sIC0gb3B0aW9uczogY2FiYWM9MSByZWY9MyBkZWJsb2NrPTE6MDowIGFuYWx5c2U9MHgzOjB4MTEzIG1lPWhleCBzdWJtZT03IHBzeT0xIHBzeV9yZD0xLjAwOjAuMDAgbWl4ZWRfcmVmPTEgbWVfcmFuZ2U9MTYgY2hyb21hX21lPTEgdHJlbGxpcz0xIDh4OGRjdD0xIGNxbT0wIGRlYWR6b25lPTIxLDExIGZhc3RfcHNraXA9MSBjaHJvbWFfcXBfb2Zmc2V0PS0yIHRocmVhZHM9MyBsb29rYWhlYWRfdGhyZWFkcz0xIHNsaWNlZF90aHJlYWRzPTAgbnI9MCBkZWNpbWF0ZT0xIGludGVybGFjZWQ9MCBibHVyYXlfY29tcGF0PTAgY29uc3RyYWluZWRfaW50cmE9MCBiZnJhbWVzPTMgYl9weXJhbWlkPTIgYl9hZGFwdD0xIGJfYmlhcz0wIGRpcmVjdD0xIHdlaWdodGI9MSBvcGVuX2dvcD0wIHdlaWdodHA9MiBrZXlpbnQ9MjUwIGtleWludF9taW49MjUgc2NlbmVjdXQ9NDAgaW50cmFfcmVmcmVzaD0wIHJjX2xvb2thaGVhZD00MCByYz1jcmYgbWJ0cmVlPTEgY3JmPTIzLjAgcWNvbXA9MC42MCBxcG1pbj0wIHFwbWF4PTY5IHFwc3RlcD00IGlwX3JhdGlvPTEuNDAgYXE9MToxLjAwAIAAAAAuZYiEADv//vdOvwKbVMIqA5JXCvbKpCZZuVN8PsayF0RQf1faBegFUAAjwOr8gQAAAAtBmiRsQ3/+p4QBxwAAAAlBnkJ4hf8A84EAAAAIAZ5hdEK/AVMAAAAIAZ5jakK/AVMAAAARQZpoSahBaJlMCGf//p4QBs0AAAALQZ6GRREsL/8A84EAAAAIAZ6ldEK/AVMAAAAIAZ6nakK/AVMAAAARQZqsSahBbJlMCFf//jhAGjAAAAALQZ7KRRUsL/8A84EAAAAIAZ7pdEK/AVMAAAAIAZ7rakK/AVM=",
    "base64",
  ),
);

const binaryPath = process.env.TOKA_E2E_BINARY ?? resolve(
  `src-tauri/target/debug/toka${process.platform === "win32" ? ".exe" : ""}`,
);

let daemon: ChildProcess | undefined;

export const config: Options.Testrunner = {
  runner: "local",
  specs: ["./test/integration/owned-index.spec.ts"],
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
  mochaOpts: { timeout: 180_000 },
  onPrepare: () => {
    // The app adds the fixture folder through its own UI; the indexer daemon
    // below is what scans it, exactly like on a real machine.
    daemon = spawn(binaryPath, ["--indexer"], {
      env: { ...process.env },
      stdio: "ignore",
      detached: true,
    });
    daemon.unref();
  },
  onComplete: async () => {
    if (daemon?.pid !== undefined) {
      try {
        process.kill(-daemon.pid);
      } catch {
        daemon.kill();
      }
    }
    rmSync(homeDirectory, { recursive: true, force: true });
  },
};
