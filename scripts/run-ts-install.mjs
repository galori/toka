import { spawnSync } from "node:child_process";

// Points the indexer service at an installed `ts` and restarts the daemon
// onto it. `install --link` writes the service file (migrating the old
// toka-indexer.service away), starts the daemon, and links the binary into
// ~/.local/bin — idempotently, so updates and first installs share this
// path. An update replaced the binary under a running daemon, so the
// restart follows: `install` alone reports "already running" and leaves
// the old daemon on the previous binary. A failed setup warns but never
// fails the install: the app runs degraded and retries at startup.
export const runTsInstall = (installedTsPath) => {
  const installed = spawnSync(installedTsPath, ["install", "--link"], {
    stdio: "inherit",
  });
  if (installed.error) {
    console.warn(
      `Could not run ts install: ${installed.error.message}. The app will retry at startup.`,
    );
    return;
  }
  if (installed.status !== 0) {
    console.warn(
      `ts install exited with ${installed.status}. The app will retry at startup.`,
    );
    return;
  }
  const restarted = spawnSync(installedTsPath, ["restart"], {
    stdio: "inherit",
  });
  if (restarted.error) {
    console.warn(
      `Could not run ts restart: ${restarted.error.message}. The running indexer keeps the previous binary until the next restart.`,
    );
  } else if (restarted.status !== 0) {
    console.warn(
      `ts restart exited with ${restarted.status}. The running indexer keeps the previous binary until the next restart.`,
    );
  }
};
