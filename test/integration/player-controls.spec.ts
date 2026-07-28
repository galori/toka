// Guards against the controls silently disappearing: the overlay's markup and its
// stylesheet drifted apart once already, which left every button rendered but
// collapsed into an unusable column that overflowed the player. Presence in the
// DOM is therefore not enough — each control is also measured.

// Play and pause are one control that renames itself, so the transport is
// measured through whichever of the two names it is wearing.
const transportControls = [
  "Previous video",
  "Skip back 10 seconds",
  "Skip forward 10 seconds",
  "Next video",
];

const utilityControls = [
  "Subtitles",
  "Rotate left",
  "Rotate right",
  "Loop: playlist",
  // The heading's playlist toggle is out of reach in fullscreen, so the overlay
  // carries one of its own.
  "Playlist",
  "Enter fullscreen",
];

type Metrics = {
  width: number;
  height: number;
  insideShell: boolean;
  clipsContent: boolean;
  onTop: boolean;
  shareOfOverlayWidth: number;
};

async function metricsFor(selector: string): Promise<Metrics | undefined> {
  return browser.execute((target) => {
    const element = document.querySelector<HTMLElement>(target);
    const shell = document.querySelector<HTMLElement>(".player-shell");
    const overlay = document.querySelector<HTMLElement>(".player-controls");
    if (!element || !shell || !overlay) return undefined;
    const box = element.getBoundingClientRect();
    const shellBox = shell.getBoundingClientRect();
    const centre = document.elementFromPoint(
      box.left + box.width / 2,
      box.top + box.height / 2,
    );
    return {
      width: box.width,
      height: box.height,
      insideShell:
        box.top >= shellBox.top - 1 &&
        box.bottom <= shellBox.bottom + 1 &&
        box.left >= shellBox.left - 1 &&
        box.right <= shellBox.right + 1,
      clipsContent:
        element.scrollWidth > element.clientWidth + 1 ||
        element.scrollHeight > element.clientHeight + 1,
      onTop: element === centre || element.contains(centre),
      shareOfOverlayWidth: box.width / overlay.clientWidth,
    };
  }, selector);
}

async function expectLaidOut(selector: string): Promise<Metrics> {
  const control = await $(selector);
  await expect(control).toBeDisplayed();

  const metrics = await metricsFor(selector);
  if (!metrics)
    throw new Error(`No player shell or control found for ${selector}`);
  // Reported together so a failure names what went wrong about the layout.
  expect({
    laidOutInsideThePlayer: metrics.insideShell,
    clipsItsOwnContent: metrics.clipsContent,
    coveredByAnotherElement: !metrics.onTop,
  }).toEqual({
    laidOutInsideThePlayer: true,
    clipsItsOwnContent: false,
    coveredByAnotherElement: false,
  });
  return metrics;
}

// A pressable control. 24px is the design system's smallest interactive step.
async function expectPressable(selector: string) {
  const metrics = await expectLaidOut(selector);
  expect(metrics.width).toBeGreaterThanOrEqual(24);
  expect(metrics.height).toBeGreaterThanOrEqual(24);
}

describe("Toka player controls", () => {
  before(async () => {
    // Typed keys need the app window to hold focus, which a second WebDriver
    // session does not reliably get. Driving React's controlled input directly
    // keeps this spec about the controls rather than about window focus.
    await browser.execute(() => {
      const search = document.querySelector<HTMLInputElement>("#video-search");
      if (!search) throw new Error("The search field is missing");
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setValue?.call(search, "sample");
      search.dispatchEvent(new Event("input", { bubbles: true }));
      document.querySelector("form")?.requestSubmit();
    });
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await $("button=Play all").click();
    await $(".player-controls").waitForDisplayed();
    // The drawer opens over the right of the overlay for a playlist; close it so
    // every control is measured against the video rather than the drawer.
    await $(".playlist-toggle").click();
    await $(".playlist-drawer").waitForExist({ reverse: true });
  });

  it("shows the scrub timeline across the overlay", async () => {
    // The scrubber is a range input, not a button: the design draws it as a
    // thin bar, so it is held to spanning the overlay rather than to the
    // pressable minimum.
    const metrics = await expectLaidOut('input[aria-label="Video timeline"]');
    expect(metrics.shareOfOverlayWidth).toBeGreaterThan(0.9);
    expect(metrics.height).toBeGreaterThan(0);
  });

  for (const label of transportControls) {
    it(`shows the ${label.toLowerCase()} control`, async () => {
      await expectPressable(
        `.player-transport > button[aria-label="${label}"]`,
      );
    });
  }

  it("shows a single play/pause control", async () => {
    await expectPressable(".player-transport > button.play-button");
    // One action, one button: the pair used to sit side by side, with whichever
    // one did not apply still taking up room and still clickable.
    const names = await browser.execute(() =>
      [
        ...document.querySelectorAll(
          '.player-transport > button[aria-label="Play"], .player-transport > button[aria-label="Pause"]',
        ),
      ].map((button) => button.getAttribute("aria-label")),
    );
    expect(names).toHaveLength(1);
    expect(["Play", "Pause"]).toContain(names[0]);
  });

  for (const label of utilityControls) {
    it(`shows the ${label.toLowerCase()} control`, async () => {
      await expectPressable(`.player-utilities button[aria-label="${label}"]`);
    });
  }

  it("shows the playback speed control", async () => {
    await expectPressable(
      '.player-utilities select[aria-label="Playback speed"]',
    );
  });

  it("shows the keyboard shortcut on the controls that have one", async () => {
    const missing = await browser.execute(() =>
      [...document.querySelectorAll(".player-controls [aria-keyshortcuts]")]
        .filter((control) => {
          const hint =
            control.querySelector(".key-hint") ??
            control.parentElement?.querySelector(".key-hint");
          if (!hint) return true;
          const box = hint.getBoundingClientRect();
          return (
            box.width === 0 ||
            box.height === 0 ||
            getComputedStyle(hint).visibility === "hidden"
          );
        })
        .map(
          (control) => control.getAttribute("aria-label") ?? control.tagName,
        ),
    );
    expect(missing).toEqual([]);
  });

  it("shows the elapsed and total time", async () => {
    await expect($(".player-transport .time-display")).toHaveText(
      /^\d+:\d\d \/ \d+:\d\d$/,
    );
  });

  it("pauses and resumes from the one overlay button", async () => {
    const pill = () =>
      browser.execute(() => {
        const button = document.querySelector<HTMLElement>(
          ".player-transport button.play-button",
        );
        const box = button?.getBoundingClientRect();
        return {
          label: button?.getAttribute("aria-label") ?? "",
          glyph: button?.querySelector(".play-glyph")
            ? "play"
            : button?.querySelector(".pause-glyph")
              ? "pause"
              : "none",
          width: box?.width ?? 0,
          left: box?.left ?? 0,
        };
      });

    // Every fixture runs for about two seconds, so a looping playlist keeps
    // starting the next video underneath this test — and each start renames the
    // button on its own. Looping the one video instead leaves playing and
    // paused as the only two states, and the button as the only way between
    // them, so a rename here can only be the click that caused it.
    await $('.player-utilities button[aria-label="Loop: playlist"]').click();
    await $(
      '.player-utilities button[aria-label="Loop: this video"]',
    ).waitForExist();

    const toggle = await $(".player-transport button.play-button");
    // An advance that began before the mode changed can still start its video
    // afterwards, so paused counts only once it has held for a moment.
    await browser.waitUntil(
      async () => {
        if ((await pill()).label === "Pause") {
          await toggle.click();
          return false;
        }
        await browser.pause(500);
        return (await pill()).label === "Play";
      },
      { timeoutMsg: "The player never came to rest on Play" },
    );

    const before = await pill();
    expect(before.glyph).toBe("play");

    await toggle.click();
    // The same element swaps its name and its glyph rather than handing over to
    // a second button.
    await browser.waitUntil(async () => (await pill()).label === "Pause", {
      timeoutMsg: 'The play/pause control stayed on "Play" after being clicked',
    });
    const after = await pill();
    expect(after.glyph).toBe("pause");
    // Swapping the glyph must not resize the pill or shove the row sideways.
    expect(Math.abs(after.width - before.width)).toBeLessThanOrEqual(1);
    expect(Math.abs(after.left - before.left)).toBeLessThanOrEqual(1);

    await toggle.click();
    await browser.waitUntil(async () => (await pill()).label === "Play", {
      timeoutMsg: "The play/pause control did not come back to its first state",
    });
    await expect($(".player-controls")).toBeDisplayed();
  });
});
