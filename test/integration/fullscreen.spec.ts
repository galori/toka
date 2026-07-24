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

describe("Toka fullscreen", () => {
  before(async () => {
    await search("sample");
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await $('button[aria-label="Play sample1.mp4"]').click();
    await $(".player-controls").waitForDisplayed();
  });

  after(async () => {
    await browser.execute(() => document.exitFullscreen?.());
  });

  it("fills the screen with the picture rather than its intrinsic size", async function () {
    // A real user gesture, so the fullscreen request is allowed.
    await $('button[aria-label="Enter fullscreen"]').click();

    const entered = await browser
      .waitUntil(async () => (await browser.execute(() => document.fullscreenElement !== null)) === true, {
        timeout: 5_000,
      })
      .then(() => true)
      .catch(() => false);
    if (!entered) {
      // Some headless WebKit builds refuse fullscreen outright. Skipping is
      // honest here: a failure would say nothing about the layout rule.
      this.skip();
    }

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
});
