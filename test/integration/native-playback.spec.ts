describe("Toka native Linux playback", () => {
  it("decodes and renders a blue video frame", async () => {
    const nativeState = () => browser.execute(() => {
      const tauri = (window as typeof window & {
        __TAURI__: { core: { invoke: (command: string) => Promise<unknown> } };
      }).__TAURI__;
      return tauri.core.invoke("native_playback_state");
    }) as Promise<{
      currentTime: number;
      blueRenderCount?: number;
      frameColor?: [number, number, number];
      framebuffer?: number;
      renderCount?: number;
      renderSize?: [number, number];
    }>;
    const search = await $("#video-search");
    await search.click();
    await browser.keys("native blue");
    await browser.execute(() => document.querySelector("form")?.requestSubmit());

    const result = await $('button[aria-label="Play native-blue.mp4"]');
    await result.waitForDisplayed();
    await result.click();

    const player = await $('[aria-label="Playing native-blue.mp4"]');
    await player.waitForDisplayed();

    const time = await $(".time-display");
    await browser.pause(1_500);
    const timeText = await time.getText();

    expect(timeText).not.toMatch(/^0:00 \/ /);
    try {
      // `frameColor` is whatever the last render happened to hold, so sampling
      // it races the end of a four-second clip: the fixture really did present
      // blue, then finished, and the poll read the black frame that followed.
      // `blueRenderCount` records the same blue test at render time, so asking
      // it answers the actual question — did the framebuffer ever show blue?
      await browser.waitUntil(async () => ((await nativeState()).blueRenderCount ?? 0) > 0, {
        timeout: 5_000,
        timeoutMsg: "the native OpenGL framebuffer did not present the blue video",
      });
    } catch (error) {
      throw new Error(`${String(error)}\n[DEBUG-native-e2e] ${JSON.stringify(await nativeState())}`);
    }

    await $('button[aria-label="Pause"]').click();
    await $('button[aria-label="Play"]').click();
  });
});
