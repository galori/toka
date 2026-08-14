const imageFiles = [
  "image-fit-tall.png",
  "image-fit-narrow.png",
  "image-fit-wide.png",
  "image-fit-short.png",
];

type Rect = {
  left: number;
  top: number;
  right: number;
  bottom: number;
  width: number;
  height: number;
};

type ImageLayout = {
  shell: Rect;
  image: Rect;
  source: string;
  complete: boolean;
  naturalWidth: number;
  naturalHeight: number;
  objectFit: string;
  objectPosition: string;
};

async function searchImage(fileName: string) {
  await browser.execute((query) => {
    const field = document.querySelector<HTMLInputElement>("#video-search");
    if (!field) throw new Error("The search field is missing");
    const setValue = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )?.set;
    setValue?.call(field, query);
    field.dispatchEvent(new Event("input", { bubbles: true }));
    document.querySelector("form")?.requestSubmit();
  }, fileName);
  await browser.waitUntil(async () =>
    (await $(`button[aria-label="Play ${fileName}"]`)).isDisplayed(),
  );
}

async function imageLayout(): Promise<ImageLayout> {
  return browser.execute(() => {
    const shell = document.querySelector(".player-shell.image-player");
    const image = document.querySelector<HTMLImageElement>(".slideshow-image");
    if (!shell || !image) throw new Error("The image player is missing");
    const box = (element: Element): Rect => {
      const bounds = element.getBoundingClientRect();
      return {
        left: bounds.left,
        top: bounds.top,
        right: bounds.right,
        bottom: bounds.bottom,
        width: bounds.width,
        height: bounds.height,
      };
    };
    return {
      shell: box(shell),
      image: box(image),
      source: image.currentSrc || image.src,
      complete: image.complete,
      naturalWidth: image.naturalWidth,
      naturalHeight: image.naturalHeight,
      objectFit: getComputedStyle(image).objectFit,
      objectPosition: getComputedStyle(image).objectPosition,
    } as ImageLayout;
  });
}

function expectImageInsidePlayer(
  layout: ImageLayout,
  fileName: string,
  mode: string,
) {
  expect(layout.naturalWidth).toBeGreaterThan(0);
  expect(layout.naturalHeight).toBeGreaterThan(0);
  expect(layout.complete).toBe(true);
  expect(layout.source).toContain(fileName);
  expect(layout.shell).not.toBeNull();
  expect(layout.image).not.toBeNull();
  if (!layout.shell || !layout.image)
    throw new Error(`The ${mode} image layout is incomplete`);

  // Each fixture has a contrasting one-pixel border. Keeping the image box
  // inside the shell is what keeps that border visible instead of cropping a
  // tall or wide source at the shell's edge.
  expect(layout.image.left).toBeGreaterThanOrEqual(layout.shell.left - 1);
  expect(layout.image.top).toBeGreaterThanOrEqual(layout.shell.top - 1);
  expect(layout.image.right).toBeLessThanOrEqual(layout.shell.right + 1);
  expect(layout.image.bottom).toBeLessThanOrEqual(layout.shell.bottom + 1);
  expect(layout.objectFit).toBe("contain");
  expect(layout.objectPosition).toBe("50% 50%");
}

function inFullscreen(): Promise<boolean> {
  return browser.execute(
    () =>
      document.fullscreenElement !== null ||
      document
        .querySelector(".player-shell")
        ?.classList.contains("fullscreen") === true,
  );
}

async function leaveFullscreen() {
  if (!(await inFullscreen())) return;
  await browser.execute(() => {
    if (document.fullscreenElement) document.exitFullscreen?.();
    else
      window.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
      );
  });
  await browser.waitUntil(async () => !(await inFullscreen()));
}

describe("Toka image fitting", () => {
  before(async () => {
    await $('button[aria-label="Images"]').click();
  });

  after(async () => {
    await leaveFullscreen();
    const back = $('button[aria-label="Back to results"]');
    if (await back.isExisting()) await back.click();
    const videos = $('button[aria-label="Videos"]');
    if (
      (await videos.isExisting()) &&
      (await videos.getAttribute("aria-pressed")) !== "true"
    )
      await videos.click();
  });

  it("keeps every aspect ratio whole in every player mode", async () => {
    for (const fileName of imageFiles) {
      await searchImage(fileName);
      await $(`button[aria-label="Play ${fileName}"]`).click();
      const image = $(`img[aria-label="Playing ${fileName}"]`);
      await image.waitForDisplayed();
      let lastLayout: ImageLayout | undefined;
      try {
        await browser.waitUntil(
          async () => {
            lastLayout = await imageLayout();
            return lastLayout.naturalWidth > 0;
          },
          { timeoutMsg: `Image did not load: ${fileName}` },
        );
      } catch (error) {
        throw new Error(
          `${String(error)}\nImage diagnostics: ${JSON.stringify(lastLayout)}`,
        );
      }
      expectImageInsidePlayer(await imageLayout(), fileName, "windowed");

      await $('button[aria-label="Enter fullscreen"]').click();
      await browser.waitUntil(inFullscreen);
      await $(".fullscreen-info").waitForDisplayed();
      expectImageInsidePlayer(
        await imageLayout(),
        fileName,
        "fullscreen information",
      );

      await browser.execute(() =>
        window.dispatchEvent(
          new KeyboardEvent("keydown", { key: "f", bubbles: true }),
        ),
      );
      await $(".player-transport").waitForDisplayed();
      expectImageInsidePlayer(
        await imageLayout(),
        fileName,
        "fullscreen controls",
      );

      await leaveFullscreen();
      await $('button[aria-label="Back to results"]').click();
      await $(`button[aria-label="Play ${fileName}"]`).waitForDisplayed();
    }
  });
});
