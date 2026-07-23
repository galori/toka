import { createReadStream, statSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { delimiter, resolve, sep } from "node:path";
import type { Options } from "@wdio/types";

const fixtureServerHost = "127.0.0.1";
const fixtureServerPort = 1421;
const fixtureDirectory = resolve("test/fixtures");
let fixtureServer: Server | undefined;

function startFixtureServer(): Promise<void> {
  fixtureServer = createServer((request, response) => {
    const pathName = new URL(request.url ?? "/", `http://${fixtureServerHost}`).pathname;
    const filePath = resolve(fixtureDirectory, decodeURIComponent(pathName.slice(1)));
    if (!filePath.startsWith(fixtureDirectory + sep) || !filePath.endsWith(".mp4")) {
      response.writeHead(404).end();
      return;
    }

    const size = statSync(filePath).size;
    const range = request.headers.range?.match(/^bytes=(\d*)-(\d*)$/);
    const start = range?.[1] ? Number(range[1]) : 0;
    const end = range?.[2] ? Math.min(Number(range[2]), size - 1) : size - 1;
    if (start >= size || end < start) {
      response.writeHead(416, { "Content-Range": `bytes */${size}` }).end();
      return;
    }

    const partial = Boolean(range);
    response.writeHead(partial ? 206 : 200, {
      "Accept-Ranges": "bytes",
      "Content-Length": end - start + 1,
      "Content-Range": partial ? `bytes ${start}-${end}/${size}` : undefined,
      "Content-Type": "video/mp4",
    });
    createReadStream(filePath, { start, end }).pipe(response);
  });

  return new Promise((resolveServer, reject) => {
    fixtureServer?.once("error", reject);
    fixtureServer?.listen(fixtureServerPort, fixtureServerHost, () => resolveServer());
  });
}

function stopFixtureServer(): Promise<void> {
  return new Promise((resolveServer, reject) => {
    fixtureServer?.close((error) => error ? reject(error) : resolveServer());
  });
}

const fixturePaths = [1, 2, 3, 4, 5].map((number) =>
  resolve("test/fixtures/sample" + number + ".mp4"),
);
process.env.TOKA_E2E_VIDEOS = fixturePaths.join(delimiter);

const binaryPath = resolve(
  `src-tauri/target/debug/toka${process.platform === "win32" ? ".exe" : ""}`,
);

export const config: Options.Testrunner = {
  runner: "local",
  specs: ["./test/integration/happy-path.spec.ts"],
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
  onPrepare: startFixtureServer,
  onComplete: stopFixtureServer,
};
