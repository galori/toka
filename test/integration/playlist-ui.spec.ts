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

// Asking an engine that is not in fullscreen to leave it is an error, not a
// no-op, so an unconditional call in a cleanup hook fails the whole suite.
function leaveFullscreen() {
  return browser.execute(() => {
    if (document.fullscreenElement) document.exitFullscreen?.();
  });
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

// Labels are wrapped in .control-label and trimmed down to their cap band, so
// the span's box is the ink. Reports how far that box's middle sits from the
// middle of the control, in pixels.
async function labelOffCentre(selector: string): Promise<number> {
  const offset = await browser.execute((target) => {
    const element = document.querySelector<HTMLElement>(target);
    const label = element?.querySelector<HTMLElement>(".control-label");
    if (!element || !label) return undefined;
    const ink = label.getBoundingClientRect();
    const box = element.getBoundingClientRect();
    return (ink.top + ink.bottom) / 2 - (box.top + box.bottom) / 2;
  }, selector);
  if (offset === undefined) throw new Error(`No trimmed label found in ${selector}`);
  return Math.abs(offset);
}

// Cap-band trimming is what makes a label's ink, rather than its line box, the
// thing that gets centred. Engines without it leave the text a few pixels high;
// that is a known and accepted fallback, so the checks that depend on it say so
// rather than failing on an engine that cannot do it.
function trimsLabels(): Promise<boolean> {
  return browser.execute(() => {
    const label = document.querySelector(".control-label");
    return (
      CSS.supports("text-box-trim", "trim-both") &&
      Boolean(label) &&
      getComputedStyle(label as Element).textBoxTrim === "trim-both"
    );
  });
}

describe("Toka playlist interface", () => {
  before(async () => {
    await search("sample");
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await $("button=Play all").click();
    await $(".player-controls").waitForDisplayed();
  });

  after(leaveFullscreen);

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

  it("centres the count inside its badge on the playlist toggle", async function () {
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

    if (!(await trimsLabels())) this.skip();
    expect(await labelOffCentre(".playlist-toggle .playlist-count")).toBeLessThanOrEqual(1);
  });

  it("centres every label against the glyphs beside it", async function () {
    if (!(await trimsLabels())) this.skip();

    for (const control of [".back-button", ".playlist-toggle", ".player-controls button.play-button"]) {
      expect({ control, offCentre: await labelOffCentre(control) <= 1 }).toEqual({
        control,
        offCentre: true,
      });
    }

    // The reported case: "Space" reading higher than the triangle or the bars
    // it labels, whichever the one control is currently showing.
    const drift = await browser.execute(() => {
      const button = document.querySelector(".player-controls button.play-button");
      const label = button?.getAttribute("aria-label") ?? "missing";
      const glyph = button?.querySelector(".play-glyph, .pause-glyph")?.getBoundingClientRect();
      const hint = button?.querySelector(".key-hint")?.getBoundingClientRect();
      if (!glyph || !hint) return { label, apart: Number.POSITIVE_INFINITY };
      return { label, apart: Math.abs((glyph.top + glyph.bottom) / 2 - (hint.top + hint.bottom) / 2) };
    });
    expect({ label: drift.label, alignedWithItsGlyph: drift.apart <= 1 }).toEqual({
      label: drift.label,
      alignedWithItsGlyph: true,
    });
  });

  it("tells a disabled skip control apart from an enabled one", async () => {
    // At the head of a playlist "Previous video" is off and "Next video" is on.
    // Fading by opacity made the two nearly indistinguishable against the
    // picture, so the difference is measured as rendered luminance.
    const shades = await browser.execute(() => {
      // Painted onto black through a canvas rather than parsed: the stylesheet
      // is written in oklch, and only the engine can say what that resolves to.
      function luminance(selector: string) {
        const button = document.querySelector<HTMLElement>(selector);
        if (!button) return undefined;
        const style = getComputedStyle(button);
        const canvas = document.createElement("canvas");
        canvas.width = 1;
        canvas.height = 1;
        const context = canvas.getContext("2d");
        if (!context) return undefined;
        context.fillStyle = "#000";
        context.fillRect(0, 0, 1, 1);
        context.globalAlpha = Number(style.opacity);
        context.fillStyle = style.color;
        context.fillRect(0, 0, 1, 1);
        const [r, g, b] = context.getImageData(0, 0, 1, 1).data;
        return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
      }
      return {
        off: luminance('.player-transport button[aria-label="Previous video"]'),
        on: luminance('.player-transport button[aria-label="Next video"]'),
        disabled: document
          .querySelector('.player-transport button[aria-label="Previous video"]')
          ?.hasAttribute("disabled"),
      };
    });

    expect(shades.disabled).toBe(true);
    if (shades.off === undefined || shades.on === undefined) throw new Error("No skip controls found");
    expect(shades.on - shades.off).toBeGreaterThan(0.25);
  });

  it("gives the heading buttons a single height", async () => {
    const heights = await browser.execute(() =>
      [".back-button", ".playlist-toggle"].map(
        (selector) => document.querySelector(selector)?.getBoundingClientRect().height ?? 0,
      ),
    );
    expect(heights[0]).toBeGreaterThan(0);
    expect(Math.abs(heights[0] - heights[1])).toBeLessThanOrEqual(1);
  });

  it("draws the icon controls at a legible size", async () => {
    const icons = await browser.execute(() =>
      ["Previous video", "Next video", "Loop: playlist", "Enter fullscreen"].map((label) => {
        const box = document
          .querySelector(`.player-controls button[aria-label="${label}"]`)
          ?.querySelector("svg")
          ?.getBoundingClientRect();
        return { label, tooSmall: !box || box.width < 18 || box.height < 18 };
      }),
    );
    expect(icons).toEqual(icons.map(({ label }) => ({ label, tooSmall: false })));
  });

  it("shows one play/pause control that swaps its glyph", async () => {
    // Two buttons for one action left whichever of them did not apply sitting
    // in the row, taking up width and still clickable.
    const before = await browser.execute(() => {
      const buttons = [...document.querySelectorAll(".player-transport > button")].map((button) =>
        button.getAttribute("aria-label"),
      );
      const pill = document.querySelector<HTMLElement>(".player-transport button.play-button");
      const box = pill?.getBoundingClientRect();
      return {
        transport: buttons,
        label: pill?.getAttribute("aria-label"),
        glyph: pill?.querySelector(".play-glyph") ? "play" : pill?.querySelector(".pause-glyph") ? "pause" : "none",
        width: box?.width ?? 0,
        left: box?.left ?? 0,
      };
    });
    expect(before.transport.filter((label) => label === "Play" || label === "Pause")).toHaveLength(1);
    expect(before.glyph).toBe(before.label === "Pause" ? "pause" : "play");

    await $(".player-transport button.play-button").click();
    await browser.waitUntil(
      async () =>
        (await browser.execute(
          () => document.querySelector(".player-transport button.play-button")?.getAttribute("aria-label"),
        )) !== before.label,
      { timeoutMsg: "The play/pause control never changed state" },
    );

    const after = await browser.execute(() => {
      const pill = document.querySelector<HTMLElement>(".player-transport button.play-button");
      const box = pill?.getBoundingClientRect();
      return {
        label: pill?.getAttribute("aria-label"),
        glyph: pill?.querySelector(".play-glyph") ? "play" : pill?.querySelector(".pause-glyph") ? "pause" : "none",
        width: box?.width ?? 0,
        left: box?.left ?? 0,
      };
    });
    expect(after.glyph).toBe(after.label === "Pause" ? "pause" : "play");
    // Swapping the glyph must not resize the pill or shove the row sideways.
    expect(Math.abs(after.width - before.width)).toBeLessThanOrEqual(1);
    expect(Math.abs(after.left - before.left)).toBeLessThanOrEqual(1);

    // Leave the transport as it was found for the tests that follow.
    await $(".player-transport button.play-button").click();
    await browser.waitUntil(
      async () =>
        (await browser.execute(
          () => document.querySelector(".player-transport button.play-button")?.getAttribute("aria-label"),
        )) === before.label,
      { timeoutMsg: "The play/pause control did not come back to its first state" },
    );
  });

  it("draws the three loop states apart from one another", async () => {
    // Painted onto black rather than parsed: the stylesheet is written in oklch
    // and only the engine can say what that resolves to.
    const shade = () =>
      browser.execute(() => {
        const button = document.querySelector<HTMLElement>(".player-controls .loop-button");
        if (!button) return undefined;
        const canvas = document.createElement("canvas");
        canvas.width = 1;
        canvas.height = 1;
        const context = canvas.getContext("2d");
        if (!context) return undefined;
        context.fillStyle = getComputedStyle(button).color;
        context.fillRect(0, 0, 1, 1);
        const [r, g, b] = context.getImageData(0, 0, 1, 1).data;
        const badge = button.querySelector(".loop-one")?.getBoundingClientRect();
        return {
          label: button.getAttribute("aria-label"),
          colour: [r, g, b] as [number, number, number],
          badge: badge ? { width: badge.width, height: badge.height } : undefined,
        };
      });

    const playlist = await shade();
    expect(playlist?.label).toBe("Loop: playlist");
    expect(playlist?.badge).toBeUndefined();

    await $(".player-controls .loop-button").click();
    const one = await shade();
    expect(one?.label).toBe("Loop: this video");
    // The "1" between the arrows is what separates this state from the one
    // before it, so it has to be drawn and be big enough to read.
    if (!one?.badge) throw new Error("No 1 was drawn inside the loop icon");
    expect(one.badge.width).toBeGreaterThan(2);
    expect(one.badge.height).toBeGreaterThan(4);
    expect(one.colour).toEqual(playlist?.colour);

    await $(".player-controls .loop-button").click();
    const off = await shade();
    expect(off?.label).toBe("Loop: off");
    expect(off?.badge).toBeUndefined();
    // Off is the plain control colour, the two that are on carry the accent.
    expect(off?.colour).not.toEqual(playlist?.colour);

    await $(".player-controls .loop-button").click();
    expect((await shade())?.label).toBe("Loop: playlist");
  });

  it("keeps the chosen speed when the playlist moves to the next video", async () => {
    // The drawer sits over the right of the overlay, where the speed control
    // is, so it is closed for the tests that drive the controls.
    await $(".playlist-toggle").click();
    await $(".playlist-drawer").waitForExist({ reverse: true });

    // Driven through React's own value setter rather than WebDriver's select
    // handling, which sets the DOM value without the controlled component ever
    // seeing a change — the same approach this suite uses for the search field.
    await browser.execute(() => {
      const select = document.querySelector<HTMLSelectElement>('select[aria-label="Playback speed"]');
      if (!select) throw new Error("The playback speed control is missing");
      const setValue = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")?.set;
      setValue?.call(select, "1.5");
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    // Asserted before advancing, so a failure says whether the speed never took
    // or whether the next video threw it away.
    await expect($('select[aria-label="Playback speed"]')).toHaveValue("1.5");

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

    await leaveFullscreen();
  });

  it("runs the scrubber all the way to the end of the bar", async function () {
    // Reported as "the playlist skips the end of every clip": the engine's last
    // reported time falls short of the duration, so the bar stopped five to ten
    // percent early just before the next video started.
    const ready = await browser.execute(() => {
      const video = document.querySelector<HTMLVideoElement>("video");
      return Boolean(video) && Number.isFinite(video?.duration) && (video?.duration ?? 0) > 0;
    });
    if (!ready) this.skip();

    // Looping off, so the player holds at the end instead of racing the next
    // video's metadata into the same measurement.
    for (let click = 0; click < 2; click += 1) await $(".player-controls .loop-button").click();
    await expect($(".player-controls .loop-button")).toHaveAttribute("aria-label", "Loop: off");

    // Each step is its own round trip, so React has flushed the state behind
    // the bar before it is read.
    const readBar = () =>
      browser.execute(() => {
        const range = document.querySelector<HTMLInputElement>(".player-timeline");
        if (!range) return undefined;
        return { value: Number(range.value), max: Number(range.max), step: Number(range.step) };
      });

    // A tenth of the clip left to run: roughly what the engine last reports
    // before it gives up on a video, and where the bar used to stop.
    await browser.execute(() => {
      const video = document.querySelector<HTMLVideoElement>("video");
      if (!video) throw new Error("The video element is missing");
      video.currentTime = Math.max(0, video.duration * 0.9);
      video.dispatchEvent(new Event("timeupdate"));
    });
    const short = await readBar();
    if (!short) throw new Error("No timeline found");
    expect(short.max).toBeGreaterThan(0);
    expect(short.value).toBeLessThan(short.max);

    await browser.execute(() => {
      document.querySelector("video")?.dispatchEvent(new Event("ended"));
    });
    // The bar can only land on a step, so within one step of the end is as full
    // as a range input gets.
    await browser.waitUntil(
      async () => {
        const bar = await readBar();
        if (!bar) return false;
        return bar.max - bar.value <= (bar.step || 0.1);
      },
      { timeoutMsg: "The scrubber never reached the end of the bar" },
    );
  });
});

describe("Toka permanent playlist mode", () => {
  before(async () => {
    await $('.player-heading button[aria-label="Back to results"]').click();
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5, {
      timeoutMsg: "The results grid never came back",
    });
  });

  after(leaveFullscreen);

  it("opens the whole page of results when one of them is chosen", async () => {
    // There is no such thing as playing one video on its own any more: a tile
    // drops the viewer into the page's playlist at that tile's place.
    await $('button[aria-label="Play sample3.mp4"]').click();
    await $(".player-controls").waitForDisplayed();

    await expect($(".playlist-status")).toHaveText("Playlist video 3 of 5");
    expect((await $$(".playlist-drawer li button")).length).toBe(5);
    await expect($('.playlist-drawer button[aria-current="true"]')).toHaveText(/sample3\.mp4/);
    await expect($(".playlist-toggle")).toHaveText(/5/);

    // Both directions are reachable from the middle of the list.
    const ends = await browser.execute(() =>
      ["Previous video", "Next video"].map((label) => ({
        label,
        disabled: document
          .querySelector(`.player-transport button[aria-label="${label}"]`)
          ?.hasAttribute("disabled"),
      })),
    );
    expect(ends).toEqual([
      { label: "Previous video", disabled: false },
      { label: "Next video", disabled: false },
    ]);
  });
});
