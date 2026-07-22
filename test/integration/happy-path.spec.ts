describe("Toka happy path", () => {
  it("searches for and opens a video, then returns to the results", async () => {
    const search = await $("#video-search");
    await search.click();
    await browser.keys("happy path");
    await browser.execute(() => document.querySelector("form")?.requestSubmit());

    const result = await $('button[aria-label="Play toka-e2e-happy-path.mp4"]');
    await result.waitForDisplayed();
    await result.click();

    const player = await $('[aria-label="Playing toka-e2e-happy-path.mp4"]');
    await player.waitForDisplayed();
    await $("button=Pause").click();
    await $("button=Play").click();
    await $('button[aria-label="Back to results"]').click();

    await expect(await $('button[aria-label="Play toka-e2e-happy-path.mp4"]')).toBeDisplayed();
  });
});
