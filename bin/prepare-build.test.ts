import { readFileSync } from "node:fs";

test("packages the generated build provenance file in the Linux deb", () => {
  const config = JSON.parse(readFileSync("src-tauri/tauri.conf.json", "utf8"));

  expect(config.bundle.linux.deb.files).toMatchObject({
    "/usr/share/toka/build-info.json": "build-info.json",
  });
});
