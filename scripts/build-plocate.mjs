import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const version = "1.1.24";
const expectedSha256 =
  "e55a757af1d7efb15ea674993224da4f0258479f8f720bd3dae0925d27dc04a2";
const sourceUrl = `https://plocate.sesse.net/download/plocate-${version}.tar.gz`;
const root = path.join(fileURLToPath(new URL(".", import.meta.url)), "..");

function run(program, arguments_, options = {}) {
  const result = spawnSync(program, arguments_, {
    stdio: "inherit",
    ...options,
  });
  if (result.error) throw result.error;
  if (result.status !== 0)
    throw new Error(
      `${program} ${arguments_.join(" ")} exited with ${result.status}`,
    );
}

function hostTriple() {
  const result = spawnSync("rustc", ["-vV"], { encoding: "utf8" });
  if (result.status !== 0)
    throw new Error("rustc -vV could not determine the build target");
  const host = result.stdout.match(/^host: (.+)$/m)?.[1];
  if (!host) throw new Error("rustc did not report a host target");
  return host;
}

export async function buildPlocate(targetTriple = hostTriple()) {
  if (!targetTriple.endsWith("linux-gnu"))
    throw new Error(`Bundled plocate is Linux-only, not ${targetTriple}`);
  const builderTriple = hostTriple();
  if (targetTriple !== builderTriple)
    throw new Error(
      `Bundled plocate needs a native ${targetTriple} builder, not ${builderTriple}`,
    );

  const cacheDir = path.join(root, "src-tauri", "target", "plocate-source");
  const archive = path.join(cacheDir, `plocate-${version}.tar.gz`);
  const sourceDir = path.join(cacheDir, `plocate-${version}`);
  const buildDir = path.join(cacheDir, `build-${targetTriple}`);
  const binariesDir = path.join(root, "src-tauri", "binaries");
  const resourcesDir = path.join(root, "src-tauri", "resources");
  const outputs = [
    path.join(binariesDir, `toka-plocate-${targetTriple}`),
    path.join(binariesDir, `toka-updatedb-${targetTriple}`),
  ];
  await Promise.all([
    mkdir(cacheDir, { recursive: true }),
    mkdir(binariesDir, { recursive: true }),
    mkdir(resourcesDir, { recursive: true }),
  ]);

  let archiveBytes;
  let downloadArchive = false;
  try {
    archiveBytes = await readFile(archive);
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    downloadArchive = true;
  }
  if (downloadArchive) {
    const response = await fetch(sourceUrl);
    if (!response.ok)
      throw new Error(`Downloading plocate failed: ${response.status}`);
    archiveBytes = Buffer.from(await response.arrayBuffer());
    await writeFile(archive, archiveBytes);
  }
  const actualSha256 = createHash("sha256").update(archiveBytes).digest("hex");
  if (actualSha256 !== expectedSha256)
    throw new Error(`plocate ${version} checksum mismatch: ${actualSha256}`);

  try {
    await readFile(path.join(sourceDir, "meson.build"));
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    run("tar", ["-xzf", archive, "-C", cacheDir]);
  }

  run("meson", [
    "setup",
    buildDir,
    sourceDir,
    "--buildtype=release",
    "-Dinstall_systemd=false",
    "-Dinstall_cron=false",
  ]);
  run("meson", ["compile", "-C", buildDir, "plocate", "updatedb"]);
  await Promise.all([
    copyFile(path.join(buildDir, "plocate"), outputs[0]),
    copyFile(path.join(buildDir, "updatedb"), outputs[1]),
    copyFile(archive, path.join(resourcesDir, `plocate-${version}.tar.gz`)),
    copyFile(
      path.join(sourceDir, "COPYING"),
      path.join(resourcesDir, "plocate-COPYING"),
    ),
  ]);
  await Promise.all(outputs.map((output) => chmod(output, 0o755)));
  return outputs;
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  await buildPlocate(process.argv[2]);
}
