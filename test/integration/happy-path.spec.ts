describe("Toka playlist", () => {
  it("searches for matching videos and plays every result in order", async () => {
    const search = await $("#video-search");
    await search.click();
    await browser.keys("sample");
    await browser.execute(() => document.querySelector("form")?.requestSubmit());

    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await $("button=Play all").click();

    for (let number = 1; number <= 5; number += 1) {
      const fileName = "sample" + number + ".mp4";
      const player = await $(`video[aria-label="Playing ${fileName}"]`);
      await player.waitForDisplayed();
      await expect($(".playlist-status")).toHaveText("Playlist video " + number + " of 5");
      if (number < 5) {
        await browser.execute(() => {
          document.querySelector("video")?.dispatchEvent(new Event("ended"));
        });
      }
    }

    await $('button[aria-label="Pause"]').click();
    await $('button[aria-label="Play"]').click();
    await $("button[aria-label=\"Back to results\"]").click();
    await expect($("button=Play all")).toBeDisplayed();
  });
});
