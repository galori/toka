async function search(query: string) {
  await browser.execute((value) => {
    const field = document.querySelector<HTMLInputElement>("#video-search");
    if (!field) throw new Error("The search field is missing");
    const setValue = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )?.set;
    setValue?.call(field, value);
    field.dispatchEvent(new Event("input", { bubbles: true }));
    document.querySelector("form")?.requestSubmit();
  }, query);
}

const exactName = "2026-10-10_@sometiktoker_18938383490812348343.mp4";

// This spec stops at the fixed action boundary. Unit tests fake the host and
// verify the child process and browser calls, so CI never opens either one.
describe("Toka add-ons", () => {
  it("offers the bundled TikTok action only for matching filenames", async () => {
    await search("@sometiktoker");
    await browser.waitUntil(async () => (await $$(".video-tile")).length === 3);
    await $(`button[aria-label="Play ${exactName}"]`).click();

    const addons = $('button[aria-label="Add-ons"]');
    await addons.waitForEnabled();
    await expect(addons).toHaveAttribute("aria-keyshortcuts", "Ctrl+Shift+A");
    await browser.execute(() => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "a",
          ctrlKey: true,
          shiftKey: true,
          bubbles: true,
        }),
      );
    });

    const menu = $('[role="menu"][aria-label="Add-ons"]');
    await menu.waitForDisplayed();
    await expect(menu).toHaveText(/Open TikTok creator/);
    await expect(menu).toHaveText(/New Toka window · tiktok\.com/);

    await browser.execute(() => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
      );
    });
    await menu.waitForExist({ reverse: true });
    await $('button[aria-label="Back to results"]').click();

    await search("unrelated-addon-fixture");
    await $('button[aria-label="Play unrelated-addon-fixture.mp4"]').click();
    await expect($('button[aria-label="Add-ons"]')).toBeDisabled();
    await $('button[aria-label="Back to results"]').click();
  });
});
