// The cursor hides after it rests on the picture during fullscreen playback.
// jsdom has no real pointer or fullscreen window, so the idle delay is only
// checked end to end here, with a real pointer move and a real wait.

// Mirrors CURSOR_IDLE_DELAY in App.tsx.
const CURSOR_IDLE_DELAY_MS = 2_000;

function inFullscreen(): Promise<boolean> {
  return browser.execute(
    () =>
      document.fullscreenElement !== null ||
      document
        .querySelector(".player-shell")
        ?.classList.contains("fullscreen") === true,
  );
}

// What the pointer would show at a viewport point: the cursor style that
// applies to the element under it, which is `none` once the shell hides it.
function cursorAt(x: number, y: number) {
  return browser.execute(
    (px, py) => {
      const target = document.elementFromPoint(px, py);
      return {
        cursor: target ? getComputedStyle(target).cursor : null,
        hiddenClass:
          document
            .querySelector(".player-shell")
            ?.classList.contains("cursor-hidden") === true,
      };
    },
    x,
    y,
  );
}

describe("Toka cursor hiding", () => {
  before(async () => {
    // The app is shared with the specs before this one, which can leave a
    // player (even a fullscreen one) open over the results.
    await browser.execute(() => {
      if (document.fullscreenElement) document.exitFullscreen?.();
      else
        window.dispatchEvent(
          new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
        );
    });
    const back = $('button[aria-label="Back to results"]');
    if (await back.isExisting()) await back.click();
    await $("#video-search").waitForExist();
    await browser.execute((value) => {
      const field = document.querySelector<HTMLInputElement>("#video-search");
      if (!field) throw new Error("The search field is missing");
      Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set?.call(field, value);
      field.dispatchEvent(new Event("input", { bubbles: true }));
      document.querySelector("form")?.requestSubmit();
    }, "sample1");
    await $('button[aria-label="Play sample1.mp4"]').click();
    await $(".player-controls").waitForDisplayed();
  });

  after(async () => {
    await browser.execute(() => {
      if (document.fullscreenElement) document.exitFullscreen?.();
      else
        window.dispatchEvent(
          new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
        );
    });
  });

  it("hides the cursor once it has rested on the fullscreen picture", async function () {
    await browser.waitUntil(
      async () =>
        (await $(".player-error-state").isExisting()) ||
        (await $('button[aria-label="Enter fullscreen"]').isExisting()),
      { timeout: 5_000 },
    );
    // An engine that cannot decode the fixture never plays, so nothing hides.
    if (await $(".player-error-state").isExisting()) this.skip();

    await $('button[aria-label="Enter fullscreen"]').click();
    const entered = await browser
      .waitUntil(inFullscreen, { timeout: 5_000 })
      .then(() => true)
      .catch(() => false);
    if (!entered) this.skip();

    // The cursor only hides during playback, so wait for it to be under way
    // (the transport button offers Pause) rather than racing the player's start.
    await $('button[aria-label="Pause"]').waitForExist({
      timeoutMsg: "The video never started playing in fullscreen",
    });

    // A known spot well inside the picture, clear of the edge-triggered
    // playlist and of the controls along the bottom.
    const { x, y } = await browser.execute(() => ({
      x: Math.round(window.innerWidth / 2),
      y: Math.round(window.innerHeight / 3),
    }));
    // [DEBUG-c7a1] record what the page sees of the pointer.
    await browser.execute(() => {
      const seen: string[] = [];
      (window as unknown as { __seen: string[] }).__seen = seen;
      for (const type of ["mousemove", "pointermove", "mouseout"]) {
        window.addEventListener(type, (event) => {
          const t = event.target as Element;
          seen.push(
            `${type}@${t.tagName}.${t.className}:${(event as MouseEvent).clientX},${(event as MouseEvent).clientY}`,
          );
        });
      }
    });
    // Move in two steps so the pointer really travels, then rests at (x, y).
    await browser
      .action("pointer")
      .move({ x: x - 20, y: y - 20 })
      .move({ x, y })
      .perform();

    expect(await cursorAt(x, y)).toEqual({
      cursor: expect.not.stringMatching(/^none$/),
      hiddenClass: false,
    });

    await browser.pause(CURSOR_IDLE_DELAY_MS + 500);

    // [DEBUG-c7a1]
    const debug = await browser.execute(() => ({
      seen: (window as unknown as { __seen: string[] }).__seen.slice(-12),
      playing: document.querySelector('button[aria-label="Pause"]') !== null,
      shell: document.querySelector(".player-shell")?.className,
      under: (() => {
        const t = document.elementFromPoint(
          window.innerWidth / 2,
          window.innerHeight / 3,
        );
        return `${t?.tagName}.${t?.className}`;
      })(),
    }));
    console.log("[DEBUG-c7a1]", JSON.stringify(debug));
    expect(await cursorAt(x, y)).toEqual({ cursor: "none", hiddenClass: true });

    // Moving the pointer brings it straight back.
    await browser
      .action("pointer")
      .move({ x: x + 10, y })
      .perform();
    expect((await cursorAt(x + 10, y)).hiddenClass).toBe(false);
  });
});
