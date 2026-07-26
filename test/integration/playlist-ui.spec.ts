// These are all layout and compositing faults reported against the Linux
// build: the playlist drawer never appeared, labels sat off-centre inside
// their buttons, and the transport icons rendered thin. None of them can be
// caught in jsdom, which has no layout, so they are measured in a real window.

async function search(query: string) {
  await browser.execute((value) => {
    const field = document.querySelector<HTMLInputElement>("#video-search");
    if (!field) throw new Error("The search field is missing");
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    setValue?.call(field, value);
    field.dispatchEvent(new Event("input", { bubbles: true }));
    document.querySelector("form")?.requestSubmit();
  }, query);
}

// The file the player is showing, so a test can wait for the playlist to move
// on without hard-coding which fixture comes next.
function playingNow(): Promise<string | undefined> {
  return browser.execute(
    () =>
      document
        .querySelector('.player-shell [aria-label^="Playing "]')
        ?.getAttribute("aria-label") ?? undefined,
  );
}

async function advanceToNextVideo() {
  const before = await playingNow();
  // Clicked through the DOM: in fullscreen there is no window chrome to
  // position a synthetic pointer against.
  await browser.execute(() => {
    document
      .querySelector<HTMLButtonElement>('.player-transport button[aria-label="Next video"]')
      ?.click();
  });
  await browser.waitUntil(async () => {
    const now = await playingNow();
    return Boolean(now) && now !== before;
  }, { timeoutMsg: `The playlist never moved on from ${before}` });
}

// How far the ink of a label sits from the middle of the box around it, as a
// share of that box's height. A baseline-aligned label beside a differently
// sized one drifts well past a tenth; centred flex items land near zero.
async function labelOffCentre(selector: string): Promise<number> {
  const offset = await browser.execute((target) => {
    const element = document.querySelector<HTMLElement>(target);
    if (!element) return undefined;
    const text = [...element.childNodes].find(
      (node) => node.nodeType === Node.TEXT_NODE && node.textContent?.trim(),
    );
    if (!text) return undefined;
    const range = document.createRange();
    range.selectNodeContents(text);
    const ink = range.getBoundingClientRect();
    const box = element.getBoundingClientRect();
    return Math.abs((ink.top + ink.bottom) / 2 - (box.top + box.bottom) / 2) / box.height;
  }, selector);
  if (offset === undefined) throw new Error(`No label found in ${selector}`);
  return offset;
}

describe("Toka playlist interface", () => {
  before(async () => {
    await search("sample");
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await $("button=Play all").click();
    await $(".player-controls").waitForDisplayed();
  });

  after(async () => {
    await browser.execute(() => document.exitFullscreen?.());
  });

  it("opens the playlist drawer over the picture", async () => {
    // On Linux the mpv surface is composited above the WebView whatever the
    // z-index says, which hid the drawer completely.
    await expect($(".playlist-drawer")).toBeDisplayed();

    const placement = await browser.execute(() => {
      const aside = document.querySelector<HTMLElement>(".playlist-drawer");
      const shell = document.querySelector<HTMLElement>(".player-shell");
      if (!aside || !shell) return undefined;
      const box = aside.getBoundingClientRect();
      const shellBox = shell.getBoundingClientRect();
      const centre = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
      return {
        width: box.width,
        height: box.height,
        onTop: aside === centre || aside.contains(centre),
        insideShell: box.right <= shellBox.right + 1 && box.top >= shellBox.top - 1,
      };
    });
    if (!placement) throw new Error("No playlist drawer or player shell found");

    expect(placement.width).toBeGreaterThan(100);
    expect(placement.height).toBeGreaterThan(100);
    expect({ coveredByAnotherElement: !placement.onTop, insideThePlayer: placement.insideShell }).toEqual({
      coveredByAnotherElement: false,
      insideThePlayer: true,
    });
  });

  it("lists every video in the drawer with the current one marked", async () => {
    expect((await $$(".playlist-drawer li button")).length).toBe(5);
    await expect($('.playlist-drawer button[aria-current="true"]')).toHaveText(/sample1\.mp4/);
  });

  it("keeps every player control clear of the open drawer", async () => {
    // The drawer is opaque and absolutely placed, so an overlay that runs the
    // full width of the shell puts its right-hand controls behind it.
    const buried = await browser.execute(() =>
      [...document.querySelectorAll<HTMLElement>(".player-controls button, .player-controls select")]
        .filter((control) => {
          const box = control.getBoundingClientRect();
          const centre = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
          return !(control === centre || control.contains(centre));
        })
        .map((control) => control.getAttribute("aria-label") ?? control.tagName),
    );
    expect(buried).toEqual([]);
  });

  it("centres the count inside its badge on the playlist toggle", async () => {
    expect(await labelOffCentre(".playlist-toggle .playlist-count")).toBeLessThan(0.1);

    const badge = await browser.execute(() => {
      const count = document.querySelector<HTMLElement>(".playlist-toggle .playlist-count");
      if (!count) return undefined;
      const box = count.getBoundingClientRect();
      return { width: box.width, height: box.height };
    });
    if (!badge) throw new Error("No playlist count badge found");

    expect(badge.height).toBeGreaterThanOrEqual(16);
    // The design draws it as a disc, so it can never be taller than it is wide.
    expect(badge.width).toBeGreaterThanOrEqual(badge.height - 1);
  });

  it("centres the labels inside the heading buttons", async () => {
    expect(await labelOffCentre(".back-button")).toBeLessThan(0.1);
    expect(await labelOffCentre(".playlist-toggle")).toBeLessThan(0.1);
  });

  it("draws the icon controls at a legible size", async () => {
    const icons = await browser.execute(() =>
      ["Previous video", "Next video", "Loop playlist", "Enter fullscreen"].map((label) => {
        const box = document
          .querySelector(`.player-controls button[aria-label="${label}"]`)
          ?.querySelector("svg")
          ?.getBoundingClientRect();
        return { label, tooSmall: !box || box.width < 18 || box.height < 18 };
      }),
    );
    expect(icons).toEqual(icons.map(({ label }) => ({ label, tooSmall: false })));
  });

  it("keeps the chosen speed when the playlist moves to the next video", async () => {
    // The drawer sits over the right of the overlay, where the speed control
    // is, so it is closed for the tests that drive the controls.
    await $(".playlist-toggle").click();
    await $(".playlist-drawer").waitForExist({ reverse: true });

    const speed = await $('select[aria-label="Playback speed"]');
    await speed.selectByAttribute("value", "1.5");
    await advanceToNextVideo();

    await expect($('select[aria-label="Playback speed"]')).toHaveValue("1.5");
    await browser.waitUntil(
      async () => (await browser.execute(() => document.querySelector("video")?.playbackRate)) === 1.5,
      { timeoutMsg: "The next video did not inherit the chosen playback speed" },
    );
  });

  it("stays fullscreen when the playlist moves to the next video", async function () {
    // A real user gesture, so the fullscreen request is allowed.
    await $('.player-controls button[aria-label="Enter fullscreen"]').click();
    const entered = await browser
      .waitUntil(async () => (await browser.execute(() => document.fullscreenElement !== null)) === true, {
        timeout: 5_000,
      })
      .then(() => true)
      .catch(() => false);
    if (!entered) {
      // Some headless WebKit builds refuse fullscreen outright; failing here
      // would say nothing about whether the player holds on to it.
      this.skip();
    }

    await advanceToNextVideo();

    // The shell is the element the browser promoted; replacing it for every
    // video is what dropped the window back out of fullscreen.
    expect(
      await browser.execute(() => document.fullscreenElement === document.querySelector(".player-shell")),
    ).toBe(true);
    await expect($('.player-controls button[aria-label="Exit fullscreen"]')).toExist();

    await browser.execute(() => document.exitFullscreen?.());
  });
});
