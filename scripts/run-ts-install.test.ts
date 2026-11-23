import { chmod, mkdtemp, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { runTsInstall } from "./run-ts-install.mjs";

describe("runTsInstall", () => {
  const writeFakeTs = async (dir: string, behavior: string) => {
    const tsPath = path.join(dir, "ts");
    const logPath = path.join(dir, "calls.log");
    await writeFile(
      tsPath,
      `#!/bin/sh\necho "$*" >> "${logPath}"\n${behavior}\n`,
    );
    await chmod(tsPath, 0o755);
    return { tsPath, logPath };
  };
  const calls = (logPath: string) =>
    readFile(logPath, "utf8").then((text) =>
      text.split("\n").filter((line) => line.length > 0),
    );

  it("installs the service with a shell link, then restarts onto the new binary", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-restart-"));
    const { tsPath, logPath } = await writeFakeTs(root, "exit 0");

    runTsInstall(tsPath);

    // `install --link` owns the ~/.local/bin/ts link; the restart moves a
    // running daemon from the previous binary onto this one.
    expect(await calls(logPath)).toEqual(["install --link", "restart"]);
  });

  it("still restarts when install succeeds with a link warning", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-restart-"));
    const { tsPath, logPath } = await writeFakeTs(
      root,
      'if [ "$1" = install ]; then echo "warning: could not link ~/.local/bin/ts: a regular file is in the way"; fi\nexit 0',
    );

    runTsInstall(tsPath);

    // A blocked link warns but exits 0: the service installed, so the
    // restart still moves a running daemon onto the new binary.
    expect(await calls(logPath)).toEqual(["install --link", "restart"]);
  });

  it("skips the restart when the install itself failed", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-restart-"));
    const { tsPath, logPath } = await writeFakeTs(
      root,
      'if [ "$1" = install ]; then exit 1; fi\nexit 0',
    );

    runTsInstall(tsPath);

    expect(await calls(logPath)).toEqual(["install --link"]);
  });

  it("warns but never throws when the restart fails", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-restart-"));
    const { tsPath, logPath } = await writeFakeTs(
      root,
      'if [ "$1" = restart ]; then exit 1; fi\nexit 0',
    );

    expect(() => runTsInstall(tsPath)).not.toThrow();
    expect(await calls(logPath)).toEqual(["install --link", "restart"]);
  });

  it("warns but never throws when ts cannot run at all", async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "toka-ts-missing-"));

    expect(() => runTsInstall(path.join(root, "ts"))).not.toThrow();
  });
});
