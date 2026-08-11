import { ThumbnailQueue } from "./thumbnailQueue";

test("limits thumbnail work and drains queued results in order", async () => {
  const releases = new Map<string, (path: string) => void>();
  const load = vi.fn(
    (resultId: string) =>
      new Promise<string>((resolve) => releases.set(resultId, resolve)),
  );
  const queue = new ThumbnailQueue(load, 2);

  const first = queue.load("first");
  const second = queue.load("second");
  const third = queue.load("third");

  expect(load).toHaveBeenCalledTimes(2);
  expect(load).toHaveBeenNthCalledWith(1, "first");
  expect(load).toHaveBeenNthCalledWith(2, "second");

  releases.get("first")?.("/tmp/first.jpg");
  await expect(first).resolves.toBe("/tmp/first.jpg");
  await vi.waitFor(() => expect(load).toHaveBeenCalledTimes(3));
  expect(load).toHaveBeenNthCalledWith(3, "third");

  releases.get("second")?.("/tmp/second.jpg");
  releases.get("third")?.("/tmp/third.jpg");
  await expect(second).resolves.toBe("/tmp/second.jpg");
  await expect(third).resolves.toBe("/tmp/third.jpg");
});

test("shares one queued request for the same result", async () => {
  let release!: (path: string) => void;
  const load = vi.fn(
    () =>
      new Promise<string>((resolve) => {
        release = resolve;
      }),
  );
  const queue = new ThumbnailQueue(load, 2);

  const first = queue.load("same");
  const second = queue.load("same");

  expect(second).toBe(first);
  expect(load).toHaveBeenCalledTimes(1);

  release("/tmp/same.jpg");
  await expect(first).resolves.toBe("/tmp/same.jpg");
  await expect(queue.load("same")).resolves.toBe("/tmp/same.jpg");
  expect(load).toHaveBeenCalledTimes(1);
});
