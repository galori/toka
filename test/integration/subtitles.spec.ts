// `test/fixtures/sample1.en.srt` sits beside `sample1.mp4`, so this exercises
// the whole sidecar path: Rust detecting the file, converting it to WebVTT, and
// the player attaching it as a text track.

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

describe("Toka subtitles", () => {
  before(async () => {
    await search("sample");
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await $('button[aria-label="Play sample1.mp4"]').click();
    await $(".player-controls").waitForDisplayed();
  });

  it("turns the sidecar subtitle on and off from the overlay", async () => {
    const toggle = await $('.player-utilities button[aria-label="Subtitles"]');
    await expect(toggle).toBeDisplayed();
    await expect(toggle).toBeEnabled();
    await expect(toggle).toHaveAttribute("aria-keyshortcuts", "S");
    await expect(toggle).toHaveAttribute("aria-pressed", "false");

    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-pressed", "true");

    const track = await $("video track");
    await expect(track).toExist();
    await expect(track).toHaveAttribute("label", "EN");
    await expect(track).toHaveAttribute("srclang", "en");

    // Rust converts SRT's comma-separated milliseconds into WebVTT's periods.
    const cues = await browser.execute(() => {
      const source = document.querySelector("video track")?.getAttribute("src") ?? "";
      return decodeURIComponent(source.replace(/^data:text\/vtt;charset=utf-8,/, ""));
    });
    expect(cues).toContain("WEBVTT");
    expect(cues).toContain("00:00:00.000 --> 00:00:02.000");

    // The media engine has to accept the track, not merely be handed one.
    await browser.waitUntil(
      async () =>
        (await browser.execute(() => {
          const tracks = document.querySelector("video")?.textTracks;
          const track = tracks?.[0];
          return track?.mode === "showing" && (track.cues?.length ?? 0) > 0;
        })) === true,
      { timeout: 5_000, timeoutMsg: "the media engine never parsed the sidecar subtitle" },
    );

    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-pressed", "false");
    await expect($$("video track")).toBeElementsArrayOfSize(0);
  });
});
