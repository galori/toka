describe("Toka native Linux playback", () => {
  it("decodes and renders a blue video frame", async () => {
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
    await browser.waitUntil(async () => {
      const state = await browser.execute(() => {
        const tauri = (window as typeof window & {
          __TAURI__: { core: { invoke: (command: string) => Promise<unknown> } };
        }).__TAURI__;
        return tauri.core.invoke("native_playback_state");
      }) as { frameColor?: [number, number, number] };
      const [red = 0, green = 0, blue = 0] = state.frameColor ?? [];
      return blue > 180 && blue > red * 2 && blue > green * 2;
    }, {
      timeout: 5_000,
      timeoutMsg: "the native OpenGL framebuffer did not present the blue video",
    });

    await $('button[aria-label="Pause"]').click();
    await $('button[aria-label="Play"]').click();
  });
});
