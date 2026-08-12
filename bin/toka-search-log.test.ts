import { describe, expect, test } from "vitest";
import {
  defaultSearchLogPath,
  formatSearchLog,
  parseSearchLogLines,
} from "./toka-search-log.mjs";

describe("toka-search-log", () => {
  test("uses Toka's platform log locations", () => {
    expect(
      defaultSearchLogPath({ platform: "linux", home: "/home/test", env: {} }),
    ).toBe("/home/test/.local/share/app.toka.desktop/logs/search.log");
    expect(
      defaultSearchLogPath({
        platform: "darwin",
        home: "/Users/test",
        env: {},
      }),
    ).toBe("/Users/test/Library/Logs/app.toka.desktop/search.log");
    expect(
      defaultSearchLogPath({
        platform: "linux",
        home: "/home/test",
        env: { XDG_DATA_HOME: "/data" },
      }),
    ).toBe("/data/app.toka.desktop/logs/search.log");
  });

  test("renders query and command columns from JSON lines", () => {
    const lines = [
      JSON.stringify({
        kind: "command",
        timestamp: 12,
        query: "summer vacation",
        command: "plocate --existing -- vacation",
      }),
      JSON.stringify({
        kind: "error",
        timestamp: 13,
        query: "broken",
        error: "plocate failed",
      }),
    ];
    expect(parseSearchLogLines(lines)).toHaveLength(2);
    const output = formatSearchLog(lines);
    expect(output).toContain("QUERY");
    expect(output).toContain("COMMAND");
    expect(output).toContain("summer vacation");
    expect(output).toContain("plocate --existing -- vacation");
    expect(output).toContain("ERROR: plocate failed");
  });
});
