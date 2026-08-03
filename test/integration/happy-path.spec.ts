async function playerDiagnostics(fileName: string) {
  return browser.execute((expectedFileName) => {
    const windowWithMediaError = window as Window & {
      tokaMediaError?: Record<string, unknown>;
      tokaAssetResponse?: Record<string, unknown>;
    };
    const video = document.querySelector<HTMLVideoElement>(`video[aria-label="Playing ${expectedFileName}"]`);
    const player = document.querySelector(".player-view");
    const alert = document.querySelector('[role="alert"]');
    const style = video ? getComputedStyle(video) : undefined;
    const bounds = video?.getBoundingClientRect();
    return {
      expectedFileName,
      assetResponse: windowWithMediaError.tokaAssetResponse,
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
    // Results come back shuffled, so which video is played first is not
    // "sample1.mp4" but whatever the grid is showing at the top. The playlist
    // plays in the order on screen, which is what this reads off it.
    const expectedOrder = await browser.execute(() =>
      [...document.querySelectorAll<HTMLElement>(".video-tile .video-name")].map(
        (name) => name.textContent?.trim() ?? "",
      ),
    );
    expect(expectedOrder).toHaveLength(5);
    expect([...expectedOrder].sort()).toEqual([1, 2, 3, 4, 5].map((number) => `sample${number}.mp4`));

    await browser.execute(() => {
      const windowWithMediaError = window as Window & {
        tokaMediaError?: Record<string, unknown>;
        tokaAssetResponse?: Record<string, unknown>;
      };
      windowWithMediaError.tokaMediaError = undefined;
      windowWithMediaError.tokaAssetResponse = undefined;
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
        void fetch(target.currentSrc)
          .then((response) => {
            windowWithMediaError.tokaAssetResponse = {
              contentLength: response.headers.get("content-length"),
              contentType: response.headers.get("content-type"),
              ok: response.ok,
              status: response.status,
              supportsRanges: response.headers.get("accept-ranges"),
            };
          })
          .catch((reason: unknown) => {
            windowWithMediaError.tokaAssetResponse = { fetchError: String(reason) };
          });
      }, true);
    });
    await $("button=Play all").click();

    for (let number = 1; number <= 5; number += 1) {
      const fileName = expectedOrder[number - 1];
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

    // One control for both directions now, so it is clicked twice rather than
    // one button being clicked after the other.
    await $("button.play-button").click();
    await $("button.play-button").click();
    await $("button[aria-label=\"Back to results\"]").click();
    await expect($("button=Play all")).toBeDisplayed();
  });
});
