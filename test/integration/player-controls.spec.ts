// Guards against the controls silently disappearing: the overlay's markup and its
// stylesheet drifted apart once already, which left every button rendered but
// collapsed into an unusable column that overflowed the player. Presence in the
// DOM is therefore not enough — each control is also measured.

const transportControls = [
  "Previous video",
  "Skip back 10 seconds",
  "Play",
  "Pause",
  "Skip forward 10 seconds",
  "Next video",
];

const utilityControls = [
  "Subtitles",
  "Rotate left",
  "Rotate right",
  "Loop playlist",
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
    const centre = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    return {
      width: box.width,
      height: box.height,
      insideShell:
        box.top >= shellBox.top - 1 &&
        box.bottom <= shellBox.bottom + 1 &&
        box.left >= shellBox.left - 1 &&
        box.right <= shellBox.right + 1,
      clipsContent: element.scrollWidth > element.clientWidth + 1 || element.scrollHeight > element.clientHeight + 1,
      onTop: element === centre || element.contains(centre),
      shareOfOverlayWidth: box.width / overlay.clientWidth,
    };
  }, selector);
}

async function expectLaidOut(selector: string): Promise<Metrics> {
  const control = await $(selector);
  await expect(control).toBeDisplayed();

  const metrics = await metricsFor(selector);
  if (!metrics) throw new Error(`No player shell or control found for ${selector}`);
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
      const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
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
      await expectPressable(`.player-transport > button[aria-label="${label}"]`);
    });
  }

  for (const label of utilityControls) {
    it(`shows the ${label.toLowerCase()} control`, async () => {
      await expectPressable(`.player-utilities button[aria-label="${label}"]`);
    });
  }

  it("shows the playback speed control", async () => {
    await expectPressable('.player-utilities select[aria-label="Playback speed"]');
  });

  it("shows the elapsed and total time", async () => {
    await expect($(".player-transport .time-display")).toHaveText(/^\d+:\d\d \/ \d+:\d\d$/);
  });

  it("pauses and resumes from the overlay buttons", async () => {
    await $('.player-transport button[aria-label="Pause"]').click();
    await $('.player-transport button[aria-label="Play"]').click();
    await expect($(".player-controls")).toBeDisplayed();
  });
});
