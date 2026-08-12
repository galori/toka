#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

export function defaultSearchLogPath({
  platform = process.platform,
  env = process.env,
  home = homedir(),
} = {}) {
  if (platform === "darwin") {
    return path.join(home, "Library", "Logs", "app.toka.desktop", "search.log");
  }
  const dataHome = env.XDG_DATA_HOME || path.join(home, ".local", "share");
  return path.join(dataHome, "app.toka.desktop", "logs", "search.log");
}

export function parseSearchLogLines(lines) {
  return lines.flatMap((line) => {
    try {
      const entry = JSON.parse(line);
      return entry && typeof entry === "object" ? [entry] : [];
    } catch {
      return [];
    }
  });
}

function safeCell(value) {
  return String(value ?? "")
    .replace(/[\t\r\n]+/g, " ")
    .trim();
}

export function formatSearchLog(lines, { header = true } = {}) {
  const entries = parseSearchLogLines(lines);
  if (!entries.length) return "";
  const rows = entries.map((entry) => [
    safeCell(entry.timestamp),
    safeCell(entry.query),
    safeCell(
      entry.command ?? (entry.error ? `ERROR: ${entry.error}` : entry.kind),
    ),
  ]);
  const widths = [0, 1, 2].map((column) =>
    Math.max(
      ["TIME", "QUERY", "COMMAND"][column].length,
      ...rows.map((row) => row[column].length),
    ),
  );
  const line = (row) =>
    row.map((cell, column) => cell.padEnd(widths[column])).join("  ");
  const output = [];
  if (header) {
    output.push(line(["TIME", "QUERY", "COMMAND"]));
    output.push(widths.map((width) => "-".repeat(width)).join("  "));
  }
  output.push(...rows.map(line));
  return output.join("\n");
}

export async function printSearchLog(pathname, state) {
  let contents;
  try {
    contents = await readFile(pathname, "utf8");
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    return;
  }
  const lines = contents.split(/\r?\n/).filter(Boolean);
  if (lines.length < state.seenLines) state.seenLines = 0;
  const fresh = lines.slice(state.seenLines);
  if (fresh.length) {
    const output = formatSearchLog(fresh, { header: state.seenLines === 0 });
    if (output) process.stdout.write(`${output}\n`);
  }
  state.seenLines = lines.length;
}

async function followSearchLog(pathname) {
  const state = { seenLines: 0 };
  await printSearchLog(pathname, state);
  const interval = setInterval(() => {
    void printSearchLog(pathname, state).catch((error) => {
      console.error(`toka-search-log: ${error.message}`);
    });
  }, 500);
  await new Promise((resolve) => {
    const stop = () => {
      clearInterval(interval);
      resolve();
    };
    process.once("SIGINT", stop);
    process.once("SIGTERM", stop);
  });
}

async function main() {
  const args = process.argv.slice(2);
  const follow = args.includes("--follow") || args.includes("-f");
  const positional = args.filter((arg) => !["--follow", "-f"].includes(arg));
  if (positional.includes("--help") || positional.includes("-h")) {
    console.log("Usage: toka-search-log [--follow] [path]");
    return;
  }
  if (positional.length > 1) throw new Error("Expected one search-log path.");
  const pathname = positional[0] ?? defaultSearchLogPath();
  if (follow) await followSearchLog(pathname);
  else await printSearchLog(pathname, { seenLines: 0 });
}

if (
  path.resolve(fileURLToPath(import.meta.url)) ===
  path.resolve(process.argv[1] ?? "")
) {
  main().catch((error) => {
    console.error(`toka-search-log: ${error.message}`);
    process.exit(1);
  });
}
