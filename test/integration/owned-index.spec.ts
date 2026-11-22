const videosDirectory = process.env.TOKA_E2E_OWNED_VIDEOS as string;

async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  return browser.execute(
    async (invocation: { command: string; args?: Record<string, unknown> }) => {
      const tauri = (window as unknown as { __TAURI__: { core: { invoke: <R>(command: string, args?: Record<string, unknown>) => Promise<R> } } }).__TAURI__;
      return tauri.core.invoke<T>(invocation.command, invocation.args);
    },
    { command, args },
  ) as Promise<T>;
}

describe("Toka owned index", () => {
  it("indexes a folder, searches it, and plays", async () => {
    // Seed the search folder through the app's own command, the way the
    // setup screen does. The indexer daemon (started by the wdio config with
    // the same isolated home directory) scans it in the background.
    await invoke("add_index_folder", { path: videosDirectory });

    // The setup screen lists the folder once the app picks it up; continuing
    // from there reaches the search screen.
    const startSearching = await $('button[aria-label="Start searching"]');
    await startSearching.waitForDisplayed({ timeout: 60_000 });
    await startSearching.click();

    const search = await $("#video-search");
    await search.click();
    await browser.keys("happy path");
    await browser.execute(() => document.querySelector("form")?.requestSubmit());

    const result = await $('button[aria-label="Play toka-e2e-happy-path.mp4"]');
    await result.waitForDisplayed({ timeout: 120_000 });
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
