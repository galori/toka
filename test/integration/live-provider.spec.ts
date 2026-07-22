describe("Toka live search provider", () => {
  it("finds the indexed family vacation fixture", async () => {
    const search = await $("#video-search");
    await search.click();
    await browser.keys("family vacation");
    await browser.execute(() => document.querySelector("form")?.requestSubmit());

    const result = await $('button[aria-label="Play family_vacation.mp4"]');
    await result.waitForDisplayed();
    await expect(result).toBeDisplayed();
  });
});
