import { createReadStream, statSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { basename } from "node:path";

export const fixtureServerHost = "127.0.0.1";
export const fixtureServerPort = 1421;
let fixtureServer: Server | undefined;

export function startFixtureServer(filePaths: string[]): Promise<void> {
  const fixtures = new Map(filePaths.map((filePath) => [basename(filePath), filePath]));
  fixtureServer = createServer((request, response) => {
    const pathName = new URL(request.url ?? "/", `http://${fixtureServerHost}`).pathname;
    const filePath = fixtures.get(decodeURIComponent(pathName.slice(1)));
    if (!filePath) {
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
    const headers: Record<string, string | number> = {
      "Accept-Ranges": "bytes",
      "Content-Length": end - start + 1,
      "Content-Type": "video/mp4",
    };
    if (partial) headers["Content-Range"] = `bytes ${start}-${end}/${size}`;
    response.writeHead(partial ? 206 : 200, headers);
    createReadStream(filePath, { start, end }).pipe(response);
  });

  return new Promise((resolveServer, reject) => {
    fixtureServer?.once("error", reject);
    fixtureServer?.listen(fixtureServerPort, fixtureServerHost, resolveServer);
  });
}

export function stopFixtureServer(): Promise<void> {
  return new Promise((resolveServer, reject) => {
    fixtureServer?.close((error) => error ? reject(error) : resolveServer());
  });
}
