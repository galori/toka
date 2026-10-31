import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const [updatedb, plocate, legacyUpdatedb] = process.argv.slice(2);
if (!updatedb || !plocate) {
  throw new Error(
    "Usage: test-plocate-hidden.mjs <updatedb> <plocate> [legacy-updatedb]",
  );
}

const directory = await mkdtemp(path.join(os.tmpdir(), "toka-hidden-"));
try {
  const root = path.join(directory, "media");
  const database = path.join(directory, "index.db");
  for (const name of [
    "visible.folder",
    ".worktrees/nested",
    "visible.folder/.private",
  ]) {
    await mkdir(path.join(root, name), { recursive: true });
  }
  for (const name of [
    "visible.folder/clip.mp4",
    ".hidden.mp4",
    ".worktrees/nested/clip.mp4",
    "visible.folder/.private/clip.mp4",
    "visible.folder/.hidden.jpg",
  ]) {
    await writeFile(path.join(root, name), "media");
  }
  const update = (program, searchRoot = root) => {
    const result = spawnSync(
      program,
      [
        "-l",
        "0",
        "-U",
        searchRoot,
        "-o",
        database,
        "--prunefs",
        "",
        "--prunenames",
        "",
        "--prunepaths",
        "",
        "--prune-bind-mounts",
        "no",
      ],
      { encoding: "utf8" },
    );
    assert.equal(result.status, 0, result.stderr);
  };
  const results = () => {
    const result = spawnSync(
      plocate,
      ["--database", database, "--regex", "."],
      { encoding: "utf8" },
    );
    assert.ok(result.status === 0 || result.status === 1, result.stderr);
    return result.stdout.trim().split("\n").filter(Boolean).sort();
  };
  const expected = [
    path.join(root, "visible.folder"),
    path.join(root, "visible.folder/clip.mp4"),
  ];

  update(updatedb);
  assert.deepEqual(
    results(),
    expected,
    "A fresh index must exclude dot files and folders.",
  );
  update(updatedb);
  assert.deepEqual(
    results(),
    expected,
    "A cached scan must keep dot paths excluded.",
  );

  if (legacyUpdatedb) {
    await rm(database);
    update(legacyUpdatedb);
    assert.ok(results().some((name) => name.includes("/.hidden")));
    update(updatedb);
    assert.deepEqual(
      results(),
      expected,
      "A rebuild must remove hidden entries from a legacy index.",
    );
  }

  update(updatedb, path.join(root, ".worktrees"));
  assert.deepEqual(
    results(),
    [],
    "A hidden search root must produce an empty index.",
  );
  console.log("Hidden-path index tests passed.");
} finally {
  await rm(directory, { recursive: true, force: true });
}
