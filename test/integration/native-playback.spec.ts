describe("Toka native Linux playback", () => {
  it("renders a video frame without covering the player controls", async () => {
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
    // Retries (see wdio.native.conf.ts) re-run this block in the same session,
    // where a previous attempt left the app in the player. Return to search
    // first so the retry starts from the same state as the first attempt.
    await browser.execute(() => {
      const back = document.querySelector<HTMLButtonElement>('button[aria-label="Back to results"]');
      back?.click();
    });
    const search = await $("#video-search");
    await search.waitForDisplayed();
    await browser.execute(() => {
      const field = document.querySelector<HTMLInputElement>("#video-search");
      const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
      setValue?.call(field, "native blue");
      field?.dispatchEvent(new Event("input", { bubbles: true }));
      document.querySelector("form")?.requestSubmit();
    });

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
    // Log the final state on success too, so passing and failing launches can
    // be compared (tracked in #57).
    console.log(`[DEBUG-native-e2e-pass] ${JSON.stringify(await nativeState())}`);

    const layout = await browser.execute(() => {
      const surface = document.querySelector<HTMLElement>(".native-video")?.getBoundingClientRect();
      const controls = document.querySelector<HTMLElement>(".player-controls")?.getBoundingClientRect();
      if (!surface || !controls) return undefined;
      return {
        surfaceHeight: surface.height,
        controlsTopWithinSurface: controls.top - surface.top,
        devicePixelRatio: window.devicePixelRatio,
      };
    });
    if (!layout) throw new Error("The native video surface or player controls are missing");
    const renderSize = (await nativeState()).renderSize;
    if (!renderSize) throw new Error("The native renderer did not report its size");
    // The native GL surface intentionally keeps the full picture bounds. The
    // WebView is composited above it, so controls remain visible without
    // shrinking the video when the overlay is shown.
    expect(renderSize[1]).toBeGreaterThanOrEqual(
      Math.floor(layout.surfaceHeight * layout.devicePixelRatio) - 2,
    );
    expect(layout.controlsTopWithinSurface).toBeGreaterThan(0);

    // Playing and pausing share one control that renames itself.
    await $("button.play-button").click();
    await $("button.play-button").click();
  });
});
