import { execFileSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { join } from "node:path";

// The player specs assert things that only hold while a video is still
// running: that the play/pause pill keeps the state a click put it in, that
// the scrubber has a duration to measure against. A looping playlist restarts
// a short clip underneath all of that, and each restart moves the very state
// being asserted on — so one fixture noticeably shorter than the rest turns
// those specs into a coin toss.
//
// `sample2.mp4` was 0.57s against its siblings' 2s. It went unnoticed for as
// long as results came back in name order and the specs reliably landed on a
// longer clip; shuffling them made it a real failure. This is the assumption
// those specs are written against, checked where it is cheap rather than
// rediscovered from a flake.
// Vitest runs from the project root, and `import.meta.url` is not a file URL
// under the jsdom environment this suite uses.
const FIXTURES = join(process.cwd(), "test", "fixtures");
const SHORTEST_USABLE = 1.5;

function duration(file: string): number | undefined {
  try {
    const seconds = execFileSync(
      "ffprobe",
      [
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        join(FIXTURES, file),
      ],
      { encoding: "utf8" },
    );
    const parsed = Number.parseFloat(seconds.trim());
    return Number.isFinite(parsed) ? parsed : undefined;
  } catch {
    // No ffprobe here. Every machine that runs the specs this protects has
    // one, so the check still holds where it matters.
    return undefined;
  }
}

test("every playlist fixture runs long enough for the player specs", () => {
  const videos = readdirSync(FIXTURES).filter((file) =>
    /^sample\d+\.mp4$/.test(file),
  );
  expect(videos.length).toBeGreaterThan(0);

  for (const file of videos) {
    const seconds = duration(file);
    if (seconds === undefined) return;
    expect(
      seconds,
      `${file} is too short for the player specs to measure against`,
    ).toBeGreaterThan(SHORTEST_USABLE);
  }
});
