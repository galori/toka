#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

function command(commandName, args) {
  const result = spawnSync(commandName, args, { encoding: "utf8" });
  if (result.status !== 0) throw result.error ?? new Error(`${commandName} failed`);
  return result.stdout.trim();
}

export function formatAge(minutes) {
  if (minutes < 60) return `${minutes} minutes`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} hours (${minutes} minutes)`;
  const days = Math.floor(hours / 24);
  return `${days} days (${hours} hours)`;
}

function main() {
  const packageName = "toka";
  const version = command("dpkg-query", ["-W", "-f=${Version}", packageName]);
  const files = command("dpkg-query", ["-L", packageName]).split("\n");
  const metadataPath = files.find((path) => path.endsWith("/build-info.json"));
  if (!metadataPath || !existsSync(metadataPath)) {
    throw new Error("Installed Toka package has no /build-info.json provenance record.");
  }

  const info = JSON.parse(readFileSync(metadataPath, "utf8"));
  const minutes = Math.max(0, Math.floor((Date.now() - Date.parse(info.builtAt)) / 60_000));
  let behind = "unknown";
  try {
    const mainRef = command("git", ["rev-parse", "origin/main"]);
    behind = command("git", ["rev-list", "--count", `${info.gitSha}..${mainRef}`]);
  } catch {
    behind = "unknown (fetch origin/main or run this from the repository)";
  }

  console.log(`Version: ${info.version} (installed package: ${version})`);
  console.log(`Built: ${info.builtAt} (${formatAge(minutes)} ago)`);
  console.log(`Git SHA: ${info.gitSha}`);
  console.log(`Behind origin/main: ${behind} commits`);
}

if (fileURLToPath(import.meta.url) === process.argv[1]) {
  try {
    main();
  } catch (error) {
    console.error(`toka-freshness: ${error.message}`);
    process.exit(1);
  }
}
