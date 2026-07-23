async function playerDiagnostics(fileName: string) {
  return browser.execute((expectedFileName) => {
    const windowWithMediaError = window as Window & { tokaMediaError?: Record<string, unknown> };
    const video = document.querySelector<HTMLVideoElement>(`video[aria-label="Playing ${expectedFileName}"]`);
    const player = document.querySelector(".player-view");
    const alert = document.querySelector('[role="alert"]');
    const style = video ? getComputedStyle(video) : undefined;
    const bounds = video?.getBoundingClientRect();
    return {
      expectedFileName,
      capturedMediaError: windowWithMediaError.tokaMediaError,
      playerText: player?.textContent?.trim(),
      playerHtml: player?.outerHTML,
      alert: alert?.textContent?.trim(),
      video: video && {
        currentSrc: video.currentSrc,
        readyState: video.readyState,
        error: video.error?.message,
        paused: video.paused,
        display: style?.display,
        visibility: style?.visibility,
        opacity: style?.opacity,
        width: bounds?.width,
        height: bounds?.height,
      },
    };
  }, fileName);
}

describe("Toka playlist", () => {
  it("searches for matching videos and plays every result in order", async () => {
    const search = await $("#video-search");
    await search.click();
    await browser.keys("sample");
    await browser.execute(() => document.querySelector("form")?.requestSubmit());

    await browser.waitUntil(async () => (await $$(".video-tile")).length === 5);
    await browser.execute(() => {
      const windowWithMediaError = window as Window & { tokaMediaError?: Record<string, unknown> };
      windowWithMediaError.tokaMediaError = undefined;
      document.addEventListener("error", (event) => {
        const target = event.target;
        if (!(target instanceof HTMLMediaElement)) return;
        windowWithMediaError.tokaMediaError = {
          currentSrc: target.currentSrc,
          errorCode: target.error?.code,
          errorMessage: target.error?.message,
          networkState: target.networkState,
          readyState: target.readyState,
        };
      }, true);
    });
    await $("button=Play all").click();

    for (let number = 1; number <= 5; number += 1) {
      const fileName = "sample" + number + ".mp4";
      const player = await $(`video[aria-label="Playing ${fileName}"]`);
      try {
        await player.waitForDisplayed();
      } catch (error) {
        throw new Error(`${String(error)}\nPlayer diagnostics: ${JSON.stringify(await playerDiagnostics(fileName), null, 2)}`);
      }
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
