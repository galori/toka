// The reported bug was purely a layout one — the app went fullscreen while the
// video kept its intrinsic size — so it can only be caught by measuring a real
// fullscreen window. jsdom cannot do that, which is why this lives here.

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

type Box = { left: number; top: number; width: number; height: number } | null;

type Layout = {
  picture: Box;
  timeline: Box;
  drawer: Box;
  // WebDriver turns `undefined` into `null` on the way out of the page, so
  // everything here is reported as a value rather than left off.
  idle: boolean | null;
  viewport: { width: number; height: number };
};

function inFullscreen(): Promise<boolean> {
  return browser.execute(() => document.fullscreenElement !== null);
}

// Some headless WebKit builds refuse fullscreen outright, and an engine that
// cannot decode the fixture leaves the player in its error state with no
// picture to measure. Reporting either rather than throwing lets each test skip
// honestly instead of failing on something that says nothing about the layout.
async function enterFullscreen(): Promise<boolean> {
  if (await $(".player-error-state").isExisting()) return false;
  if (await inFullscreen()) return true;
  // A real user gesture, so the fullscreen request is allowed.
  await $('button[aria-label="Enter fullscreen"]').click();
  return browser
    .waitUntil(async () => (await inFullscreen()) === true, { timeout: 5_000 })
    .then(() => true)
    .catch(() => false);
}

// The overlay is driven by the pointer, and both halves of it take themselves
// away again on a timer, so the pointer is held where it is wanted rather than
// moved once — otherwise the countdown can reclaim the screen in the middle of
// a measurement.
function holdPointer(atRightEdge: boolean) {
  return browser.execute((edge) => {
    const held = window as unknown as { __tokaPointer?: number };
    if (held.__tokaPointer) window.clearInterval(held.__tokaPointer);
    const send = () =>
      window.dispatchEvent(
        new MouseEvent("mousemove", {
          clientX: edge ? window.innerWidth - 1 : Math.round(window.innerWidth / 2),
          clientY: Math.round(window.innerHeight / 2),
        }),
      );
    send();
    held.__tokaPointer = window.setInterval(send, 100);
  }, atRightEdge);
}

function releasePointer() {
  return browser.execute(() => {
    const held = window as unknown as { __tokaPointer?: number };
    if (held.__tokaPointer) window.clearInterval(held.__tokaPointer);
    held.__tokaPointer = 0;
  });
}

function layout(): Promise<Layout> {
  return browser.execute(() => {
    const rect = (element: Element | null) => {
      if (!element) return null;
      const box = element.getBoundingClientRect();
      return {
        left: Math.round(box.left),
        top: Math.round(box.top),
        width: Math.round(box.width),
        height: Math.round(box.height),
      };
    };
    const controls = document.querySelector(".player-controls");
    return {
      picture: rect(document.querySelector("video, .native-video")),
      timeline: rect(document.querySelector(".player-timeline")),
      drawer: rect(document.querySelector(".playlist-drawer")),
      idle: controls ? controls.classList.contains("idle") : null,
      viewport: { width: window.innerWidth, height: window.innerHeight },
    };
  });
}

// Reads the layout and the state it belongs to in one round trip, so a timer
// cannot change the overlay between the two.
async function layoutWhile(idle: boolean, message: string): Promise<Layout> {
  let seen: Layout | undefined;
  await browser.waitUntil(
    async () => {
      seen = await layout();
      return seen.idle === idle && Boolean(seen.picture);
    },
    { timeout: 8_000, timeoutMsg: message },
  );
  return seen as Layout;
}

describe("Toka fullscreen", () => {
  before(async () => {
    await search("sample");
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await $('button[aria-label="Play sample1.mp4"]').click();
    await $(".player-controls").waitForDisplayed();
    // Every result page is a playlist now, so the drawer opens over the right
    // of the overlay; close it so the controls are driven against the picture.
    await $(".playlist-toggle").click();
    await $(".playlist-drawer").waitForExist({ reverse: true });
  });

  after(async () => {
    await releasePointer();
    await browser.execute(() => {
      if (document.fullscreenElement) document.exitFullscreen?.();
    });
  });

  it("fills the screen with the picture rather than its intrinsic size", async function () {
    if (!(await enterFullscreen())) this.skip();

    const measurements = await browser.execute(() => {
      const picture = document.querySelector<HTMLElement>("video, .native-video");
      if (!picture) return undefined;
      const box = picture.getBoundingClientRect();
      return {
        heightShare: box.height / window.innerHeight,
        widthShare: box.width / window.innerWidth,
        intrinsicHeight: (picture as HTMLVideoElement).videoHeight ?? 0,
      };
    });

    if (!measurements) throw new Error("No video surface found in fullscreen");
    // The fixture is 320x180; before the fix the element stayed that tall in
    // the middle of the screen.
    expect(measurements.heightShare).toBeGreaterThan(0.9);
    expect(measurements.widthShare).toBeGreaterThan(0.9);
  });

  it("leaves nothing but a scrubber sliver below the picture once the pointer settles", async function () {
    if (!(await enterFullscreen())) this.skip();
    await releasePointer();

    const hidden = await layoutWhile(true, "The fullscreen overlay never collapsed onto the scrubber");
    const { picture, timeline, viewport } = hidden;
    if (!picture || !timeline) throw new Error("No picture or scrubber found in fullscreen");

    // The playlist is gone and the transport row with it, and what is left is a
    // strip a few pixels tall that the picture stops short of rather than sits
    // under — the one thing fullscreen takes off the video.
    expect(hidden.drawer).toBe(null);
    expect(await $(".player-transport").isDisplayed()).toBe(false);
    expect({ sliverHeight: timeline.height > 0 && timeline.height <= 10 }).toEqual({ sliverHeight: true });
    expect(Math.abs(timeline.top - (picture.top + picture.height))).toBeLessThanOrEqual(1);
    expect(Math.abs(timeline.top + timeline.height - viewport.height)).toBeLessThanOrEqual(1);
  });

  it("overlays the controls and the playlist without moving the picture", async function () {
    if (!(await enterFullscreen())) this.skip();
    await releasePointer();
    const hidden = await layoutWhile(true, "The fullscreen overlay never collapsed onto the scrubber");

    // Any movement brings the whole overlay back, and the scrubber goes up with
    // it — over the picture this time, not below it.
    await holdPointer(false);
    const shown = await layoutWhile(false, "Moving the pointer never brought the fullscreen controls back");
    expect(shown.picture).toEqual(hidden.picture);
    if (!shown.timeline || !shown.picture) throw new Error("No picture or scrubber found in fullscreen");
    expect(shown.timeline.top).toBeLessThan(shown.picture.top + shown.picture.height);

    // The right-hand edge of the screen slides the playlist out over the top of
    // the picture. Nothing about the video may move: this is the whole of the
    // "same resolution, location and aspect ratio" rule.
    await holdPointer(true);
    await $(".playlist-drawer").waitForExist({ timeoutMsg: "The right-hand edge never revealed the playlist" });
    const withPlaylist = await layoutWhile(false, "The fullscreen controls went away while the playlist was up");
    expect(withPlaylist.picture).toEqual(hidden.picture);
    if (!withPlaylist.drawer) throw new Error("The playlist drawer was not measured");
    expect(withPlaylist.drawer.width).toBeGreaterThan(100);
    expect(
      Math.abs(withPlaylist.drawer.left + withPlaylist.drawer.width - withPlaylist.viewport.width),
    ).toBeLessThanOrEqual(1);

    // And taking the pointer away puts everything back without moving it either.
    await releasePointer();
    await $(".playlist-drawer").waitForExist({
      reverse: true,
      timeoutMsg: "The playlist stayed up after the pointer left it",
    });
    const back = await layoutWhile(true, "The fullscreen overlay never collapsed onto the scrubber again");
    expect(back.picture).toEqual(hidden.picture);
  });
});
