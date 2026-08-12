import capabilities from "../src-tauri/capabilities/default.json";

test("allows the window close operation used by Ctrl+W", () => {
  expect(capabilities.permissions).toContain("core:window:allow-close");
});
