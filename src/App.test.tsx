import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import App from "./App";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  convertFileSrc: vi.fn((path: string) => `asset://${path}`),
}));

const invokeMock = vi.mocked(invoke);
const convertFileSrcMock = vi.mocked(convertFileSrc);

beforeEach(() => {
  invokeMock.mockReset();
  convertFileSrcMock.mockClear();
});

test("starts with a focused search field and displays submitted results", async () => {
  invokeMock.mockResolvedValueOnce({
    query: "summer vacation",
    page: 1,
    pageSize: 24,
    totalResults: 1,
    totalPages: 1,
    results: [{ id: "video-1", fileName: "Summer Vacation.mp4", extension: "mp4" }],
  });
  const user = userEvent.setup();
  render(<App />);

  const search = screen.getByRole("searchbox", { name: "Search videos" });
  expect(search).toHaveFocus();
  expect(screen.queryByRole("list", { name: "Video results" })).not.toBeInTheDocument();

  await user.type(search, "summer vacation{Enter}");

  expect(await screen.findByRole("button", { name: "Play Summer Vacation.mp4" })).toBeVisible();
  expect(invokeMock).toHaveBeenCalledWith("search_videos", {
    request: { query: "summer vacation", page: 1, pageSize: 24 },
  });
});

test("opens a selected result in the player and restores the grid on back", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));

  const video = await screen.findByLabelText("Playing clip.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  fireEvent.loadedMetadata(video);
  expect(convertFileSrcMock).toHaveBeenCalledWith("/Videos/clip.mp4");
  expect(HTMLMediaElement.prototype.play).toHaveBeenCalled();

  await user.click(screen.getByRole("button", { name: "Back to results" }));
  expect(screen.getByRole("button", { name: "Play clip.mp4" })).toBeVisible();
});

test("enters fullscreen mode for the player", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const requestFullscreen = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", { configurable: true, value: requestFullscreen });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  await user.click(await screen.findByRole("button", { name: "Enter fullscreen" }));

  expect(requestFullscreen).toHaveBeenCalledOnce();
});

test("loops a single video when loop video is enabled", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  const video = await screen.findByLabelText("Playing clip.mp4");
  const play = vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
  await user.click(screen.getByRole("button", { name: "Loop video" }));
  expect(screen.getByRole("button", { name: "Loop video" })).toHaveAttribute("aria-pressed", "true");
  fireEvent.ended(video);
  expect(play).toHaveBeenCalled();
});

test("rotates web playback clockwise and counter-clockwise", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  const video = await screen.findByLabelText("Playing clip.mp4");

  await user.click(screen.getByRole("button", { name: "Rotate right" }));
  expect(video).toHaveStyle({ transform: "rotate(90deg)" });

  await user.click(screen.getByRole("button", { name: "Rotate left" }));
  expect(video).toHaveStyle({ transform: "rotate(0deg)" });
});

test("sends the selected rotation to native playback", async () => {
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
        results: [{ id: "native-1", fileName: "native.mp4", extension: "mp4" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({ filePath: "/Videos/native.mp4", playbackBackend: "native" });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({ duration: 120, currentTime: 1, paused: false, ended: false });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play native.mp4" }));
  await user.click(await screen.findByRole("button", { name: "Rotate left" }));

  expect(invokeMock).toHaveBeenCalledWith("set_native_video_rotation", { degrees: 270 });
});

test("keyboard shortcuts control player actions without hijacking search input", async () => {
  const results = [1, 2].map((number) => ({ id: `video-${number}`, fileName: `clip-${number}.mp4`, extension: "mp4" }));
  invokeMock
    .mockResolvedValueOnce({ query: "clip", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValueOnce({ filePath: "/Videos/clip-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/clip-2.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/clip-1.mp4" });
  const requestFullscreen = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", { configurable: true, value: requestFullscreen });
  const user = userEvent.setup();
  render(<App />);

  const search = screen.getByRole("searchbox");
  await user.type(search, "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  const video = await screen.findByLabelText("Playing clip-1.mp4");

  fireEvent.keyDown(window, { key: "]" });
  expect(video).toHaveStyle({ transform: "rotate(90deg)" });
  fireEvent.keyDown(window, { key: "ArrowRight", shiftKey: true });
  expect(await screen.findByLabelText("Playing clip-2.mp4")).toBeVisible();
  fireEvent.keyDown(window, { key: "ArrowLeft", shiftKey: true });
  expect(await screen.findByLabelText("Playing clip-1.mp4")).toBeVisible();
  fireEvent.keyDown(window, { key: "l" });
  expect(screen.getByRole("button", { name: "Loop playlist" })).toHaveAttribute("aria-pressed", "true");
  fireEvent.keyDown(window, { key: "f" });
  expect(requestFullscreen).toHaveBeenCalledOnce();

  search.focus();
  fireEvent.keyDown(search, { key: "]" });
  expect(screen.getByLabelText("Playing clip-1.mp4")).toHaveStyle({ transform: "rotate(0deg)" });
});

test("loops a playlist back to its first video", async () => {
  const results = [1, 2].map((number) => ({ id: `video-${number}`, fileName: `playlist-${number}.mp4`, extension: "mp4" }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-2.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await user.click(await screen.findByRole("button", { name: "Loop playlist" }));
  fireEvent.ended(await screen.findByLabelText("Playing playlist-1.mp4"));
  fireEvent.ended(await screen.findByLabelText("Playing playlist-2.mp4"));
  expect(await screen.findByLabelText("Playing playlist-1.mp4")).toBeVisible();
});

test("paginates and reports provider failures", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 25,
      totalPages: 2,
      results: [{ id: "first", fileName: "clip-00.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({
      query: "clip",
      page: 2,
      pageSize: 24,
      totalResults: 25,
      totalPages: 2,
      results: [{ id: "last", fileName: "clip-24.mp4", extension: "mp4" }],
    })
    .mockRejectedValueOnce({
      kind: "Provider",
      message: "Recoll search could not start.",
    });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Next page" }));
  expect((await screen.findAllByText("Page 2 of 2"))[0]).toBeVisible();

  await user.clear(screen.getByRole("searchbox"));
  await user.type(screen.getByRole("searchbox"), "broken{Enter}");
  expect(await screen.findByRole("alert")).toHaveTextContent("Recoll search could not start.");
  await waitFor(() => expect(screen.getByRole("searchbox")).toHaveValue("broken"));
});

test("playlist mode advances through every search result", async () => {
  const results = [1, 2, 3].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 3,
      totalPages: 1,
      results,
    })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-2.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-3.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  const first = await screen.findByLabelText("Playing playlist-1.mp4");
  expect(screen.getByText("Playlist video 1 of 3")).toBeVisible();
  fireEvent.ended(first);
  const second = await screen.findByLabelText("Playing playlist-2.mp4");
  expect(screen.getByText("Playlist video 2 of 3")).toBeVisible();
  fireEvent.ended(second);
  const third = await screen.findByLabelText("Playing playlist-3.mp4");
  expect(screen.getByText("Playlist video 3 of 3")).toBeVisible();
  fireEvent.ended(third);

  expect(screen.getByLabelText("Playing playlist-3.mp4")).toBeVisible();
  expect(invokeMock).toHaveBeenLastCalledWith("prepare_video", { resultId: "video-3" });
});

test("opens the playlist drawer and plays a selected playlist item", async () => {
  const results = [1, 2, 3].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 3,
      totalPages: 1,
      results,
    })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-3.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  const toggle = screen.getByRole("button", { name: "Playlist 3" });
  expect(toggle).toHaveAttribute("aria-expanded", "true");
  expect(screen.getByRole("complementary", { name: "Playlist" })).toBeVisible();
  expect(screen.getByRole("button", { name: "playlist-1.mp4" })).toHaveAttribute("aria-current", "true");

  await user.click(toggle);
  expect(screen.queryByRole("complementary", { name: "Playlist" })).not.toBeInTheDocument();
  await user.click(toggle);
  await user.click(screen.getByRole("button", { name: "playlist-3.mp4" }));

  expect(await screen.findByLabelText("Playing playlist-3.mp4")).toBeVisible();
  expect(screen.getByRole("button", { name: "playlist-3.mp4" })).toHaveAttribute("aria-current", "true");
});

test("native playlist advances when libmpv reports end of file", async () => {
  const results = [1, 2].map((number) => ({
    id: `native-${number}`,
    fileName: `native-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native",
        page: 1,
        pageSize: 24,
        totalResults: 2,
        totalPages: 1,
        results,
      });
    }
    if (command === "prepare_video") {
      const resultId = (args as { resultId: string }).resultId;
      return Promise.resolve({ filePath: `/Videos/${resultId}.mp4`, playbackBackend: "native" });
    }
    if (command === "native_playback_state") {
      return Promise.resolve({ duration: 1, currentTime: 1, paused: true, ended: true });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  expect(await screen.findByLabelText("Playing native-1.mp4")).toBeVisible();
  await waitFor(() => {
    expect(invokeMock).toHaveBeenCalledWith("set_native_paused", { paused: false });
  });
  expect(await screen.findByLabelText("Playing native-2.mp4", {}, { timeout: 1_000 })).toBeVisible();
  expect(screen.getByText("Playlist video 2 of 2")).toBeVisible();
});
