import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import App, { playbackSource } from "./App";

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

afterEach(() => {
  vi.unstubAllEnvs();
});

test("uses the fixture server for the Linux web playback fallback in E2E builds", () => {
  vi.stubEnv("VITE_E2E", "1");
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue("WebKitGTK Linux");

  expect(playbackSource("/Videos/clip #1.mp4")).toBe("http://127.0.0.1:1421/clip%20%231.mp4");
  expect(convertFileSrcMock).not.toHaveBeenCalled();
});

test("retains the asset protocol for E2E builds on platforms that support it", () => {
  vi.stubEnv("VITE_E2E", "1");
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue("AppleWebKit Mac OS X");

  expect(playbackSource("/Videos/clip.mp4")).toBe("asset:///Videos/clip.mp4");
  expect(convertFileSrcMock).toHaveBeenCalledWith("/Videos/clip.mp4");
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

test("uses the overlay player controls from the design", async () => {
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

  const controls = await screen.findByLabelText("Video controls");
  expect(controls).toHaveClass("player-controls");

  // The design stacks a full-width scrubber above a single transport row, so the
  // stylesheet's .player-transport and .player-utilities rules have to find the
  // elements they lay out. Without them every control collapses to a 24px column.
  const timeline = screen.getByLabelText("Video timeline");
  expect(timeline).toHaveClass("player-timeline");
  expect(timeline.parentElement).toBe(controls);

  const transport = controls.querySelector(".player-transport");
  const utilities = controls.querySelector(".player-utilities");
  expect(transport).toBeInTheDocument();
  expect(utilities).toBeInTheDocument();

  for (const name of ["Previous video", "Skip back 10 seconds", "Play", "Pause", "Skip forward 10 seconds", "Next video"]) {
    expect(transport).toContainElement(screen.getByRole("button", { name }));
  }
  expect(transport).toContainElement(screen.getByText("0:00 / 0:00"));

  for (const name of ["Rotate left", "Rotate right", "Loop video", "Enter fullscreen"]) {
    expect(utilities).toContainElement(screen.getByRole("button", { name }));
  }
  expect(utilities).toContainElement(screen.getByRole("combobox", { name: "Playback speed" }));

  const play = screen.getByRole("button", { name: "Play" });
  expect(play).toHaveClass("play-button");
  expect(play.querySelector(".play-glyph")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Pause" }).querySelector(".pause-glyph")).toBeInTheDocument();
});

test("shows a sidecar subtitle track and turns it off again", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
        results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/clip.mp4",
        subtitles: [
          { track: 0, label: "Subtitles", language: null, webPlayable: true },
          { track: 1, label: "EN", language: "en", webPlayable: true },
        ],
      });
    }
    if (command === "subtitle_cues") return Promise.resolve("WEBVTT\n\n00:01.000 --> 00:02.000\nHi");
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));

  const toggle = await screen.findByRole("button", { name: "Subtitles" });
  expect(toggle).toHaveAttribute("aria-pressed", "false");
  expect(toggle).toHaveAttribute("aria-keyshortcuts", "S");
  expect(toggle).toBeEnabled();

  await user.click(toggle);
  await waitFor(() => expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute("aria-pressed", "true"));
  expect(invokeMock).toHaveBeenCalledWith("subtitle_cues", { resultId: "video-1", track: 0 });

  const track = document.querySelector("track");
  expect(track).toHaveAttribute("label", "Subtitles");
  expect(track?.getAttribute("src")).toContain("text/vtt");

  await user.click(screen.getByRole("button", { name: "Subtitles" }));
  expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute("aria-pressed", "false");
  expect(document.querySelector("track")).not.toBeInTheDocument();
});

test("switches between the subtitle tracks found beside the video", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
        results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/clip.mp4",
        subtitles: [
          { track: 0, label: "Subtitles", language: null, webPlayable: true },
          { track: 1, label: "EN", language: "en", webPlayable: true },
          { track: 2, label: "Styled", language: null, webPlayable: false },
        ],
      });
    }
    if (command === "subtitle_cues") return Promise.resolve("WEBVTT");
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  await user.click(await screen.findByRole("button", { name: "Subtitles" }));

  const chooser = await screen.findByRole("combobox", { name: "Subtitle track" });
  // The .ass sidecar is listed by Rust but the web engine cannot render it.
  expect([...chooser.querySelectorAll("option")].map((option) => option.textContent)).toEqual([
    "Off",
    "Subtitles",
    "EN",
  ]);

  await user.selectOptions(chooser, "1");
  await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("subtitle_cues", { resultId: "video-1", track: 1 }));
  expect(document.querySelector("track")).toHaveAttribute("srclang", "en");
});

test("toggles subtitles with the keyboard and disables the control without tracks", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4", subtitles: [] });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));

  // Visible but inert, so the feature stays discoverable when a video has none.
  const toggle = await screen.findByRole("button", { name: "Subtitles" });
  expect(toggle).toBeVisible();
  expect(toggle).toBeDisabled();

  fireEvent.keyDown(window, { key: "s" });
  expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute("aria-pressed", "false");
  expect(screen.queryByRole("combobox", { name: "Subtitle track" })).not.toBeInTheDocument();
});

test("selects and clears the mpv subtitle track for native playback", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
        results: [{ id: "native-1", fileName: "native.mkv", extension: "mkv" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({ filePath: "/Videos/native.mkv", playbackBackend: "native", subtitles: [] });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({ duration: 120, currentTime: 1, paused: false, ended: false });
    }
    if (command === "native_subtitle_tracks") {
      return Promise.resolve([{ id: 1, label: "English", external: false }]);
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play native.mkv" }));

  // mpv reports embedded tracks only once the file has loaded.
  await waitFor(() => expect(screen.getByRole("button", { name: "Subtitles" })).toBeEnabled());

  fireEvent.keyDown(window, { key: "s" });
  await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("set_native_subtitle", { id: 1 }));
  expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute("aria-pressed", "true");

  fireEvent.keyDown(window, { key: "s" });
  await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("set_native_subtitle", { id: null }));
});

test("presents a dedicated unsupported-format state", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockRejectedValueOnce(new Error("This video format or codec is not supported on this computer."));
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));

  expect(await screen.findByRole("heading", { name: "This video format isn't supported on your computer" })).toBeVisible();
  expect(screen.getByRole("button", { name: "Back to results" })).toBeVisible();
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

test("changes playback speed for web video", async () => {
  invokeMock.mockResolvedValueOnce({ query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1, results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }] }).mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup(); render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  const video = await screen.findByLabelText("Playing clip.mp4");
  await user.selectOptions(screen.getByRole("combobox", { name: "Playback speed" }), "1.5");
  expect(video).toHaveProperty("playbackRate", 1.5);
});

test("skips web playback by ten seconds", async () => {
  invokeMock.mockResolvedValueOnce({ query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1, results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }] }).mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup(); render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}"); await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  const video = await screen.findByLabelText("Playing clip.mp4"); Object.defineProperty(video, "duration", { configurable: true, value: 120 }); Object.defineProperty(video, "currentTime", { configurable: true, writable: true, value: 20 });
  fireEvent.timeUpdate(video); await user.click(screen.getByRole("button", { name: "Skip forward 10 seconds" })); expect((video as HTMLVideoElement).currentTime).toBe(30);
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
