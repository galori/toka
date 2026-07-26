describe("Toka real plocate provider", () => {
  it("searches through the real plocate provider", async () => {
    const search = await $("#video-search");
    await search.click();
    await browser.keys("happy path");
    await browser.execute(() => document.querySelector("form")?.requestSubmit());

    const result = await $('button[aria-label="Play toka-e2e-happy-path.mp4"]');
    await result.waitForDisplayed();
    await result.click();

    const player = await $('video[aria-label="Playing toka-e2e-happy-path.mp4"]');
    await player.waitForDisplayed();
    // Playing and pausing share one control that renames itself.
    await $("button.play-button").click();
    await $("button.play-button").click();
    await $('button[aria-label="Back to results"]').click();

    await expect(await $('button[aria-label="Play toka-e2e-happy-path.mp4"]')).toBeDisplayed();
  });
});
