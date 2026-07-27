import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import App, { playbackSource, shuffleVideos } from "./App";

const windowApiMock = vi.hoisted(() => ({
  isFullscreen: vi.fn(),
  setFullscreen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  convertFileSrc: vi.fn((path: string) => `asset://${path}`),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => windowApiMock,
}));

const invokeMock = vi.mocked(invoke);
const convertFileSrcMock = vi.mocked(convertFileSrc);
const randomMock = vi.spyOn(Math, "random");

test("shuffles without mutating the original video list", () => {
  const videos = [1, 2, 3].map((id) => ({ id: String(id), fileName: `${id}.mp4`, extension: "mp4" }));
  randomMock.mockReturnValueOnce(0).mockReturnValueOnce(0);
  expect(shuffleVideos(videos).map((video) => video.id)).toEqual(["2", "3", "1"]);
  expect(videos.map((video) => video.id)).toEqual(["1", "2", "3"]);
});

test("shows build provenance on the initial home screen", () => {
  render(<App />);

  expect(screen.getByRole("region", { name: "Build information" })).toHaveTextContent("Version 0.1.0");
  expect(screen.getByRole("region", { name: "Build information" })).toHaveTextContent("Built");
  expect(screen.getByRole("region", { name: "Build information" })).toHaveTextContent("Git SHA");
});

beforeEach(() => {
  invokeMock.mockReset();
  convertFileSrcMock.mockClear();
  randomMock.mockReset().mockReturnValue(0.999999);
  windowApiMock.isFullscreen.mockReset();
  windowApiMock.setFullscreen.mockReset();
  windowApiMock.isFullscreen.mockResolvedValue(false);
  windowApiMock.setFullscreen.mockResolvedValue(undefined);
});

afterEach(() => {
  Object.defineProperty(document, "fullscreenElement", {
    configurable: true,
    get: () => null,
  });
  vi.unstubAllEnvs();
  vi.unstubAllGlobals();
});

test("uses the fixture server for the Linux web playback fallback in E2E builds", () => {
  vi.stubEnv("VITE_E2E", "1");
  const userAgent = vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue("WebKitGTK Linux");

  try {
    expect(playbackSource("/Videos/clip #1.mp4")).toBe("http://127.0.0.1:1421/clip%20%231.mp4");
    expect(convertFileSrcMock).not.toHaveBeenCalled();
  } finally {
    userAgent.mockRestore();
  }
});

test("retains the asset protocol for E2E builds on platforms that support it", () => {
  vi.stubEnv("VITE_E2E", "1");
  const userAgent = vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue("AppleWebKit Mac OS X");

  try {
    expect(playbackSource("/Videos/clip.mp4")).toBe("asset:///Videos/clip.mp4");
    expect(convertFileSrcMock).toHaveBeenCalledWith("/Videos/clip.mp4");
  } finally {
    userAgent.mockRestore();
  }
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

test("offers a results shuffle control and restarts a shuffled playlist with R", async () => {
  const results = [1, 2, 3].map((number) => ({ id: `video-${number}`, fileName: `shuffle-${number}.mp4`, extension: "mp4" }));
  invokeMock.mockResolvedValueOnce({ query: "shuffle", page: 1, pageSize: 24, totalResults: 3, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/shuffle.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "shuffle{Enter}");
  const resultsButton = await screen.findByRole("button", { name: "Shuffle results" });
  expect(resultsButton).toHaveAttribute("aria-keyshortcuts", "R");
  await user.click(screen.getByRole("button", { name: "Play shuffle-2.mp4" }));
  expect(await screen.findByLabelText("Playing shuffle-2.mp4")).toBeVisible();
  expect(screen.getByRole("button", { name: "Shuffle playlist" })).toHaveAttribute("aria-keyshortcuts", "R");
  randomMock.mockReturnValueOnce(0.999999).mockReturnValueOnce(0.999999);
  fireEvent.keyDown(window, { key: "r" });
  expect(await screen.findByLabelText("Playing shuffle-1.mp4")).toBeVisible();
});

test("displays an app-generated thumbnail when search returns one", async () => {
  invokeMock.mockResolvedValueOnce({
    query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
    results: [{
      id: "video-1", fileName: "clip.mp4", extension: "mp4",
      thumbnailPath: "/tmp/toka-thumbnails/clip.jpg",
    }],
  });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");

  expect(await screen.findByRole("button", { name: "Play clip.mp4" })).toBeVisible();
  expect(screen.getByRole("button", { name: "Play clip.mp4" }).querySelector(".video-art"))
    .toHaveStyle({ backgroundImage: 'url("asset:///tmp/toka-thumbnails/clip.jpg")' });
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

  for (const name of ["Previous video", "Skip back 10 seconds", "Play", "Skip forward 10 seconds", "Next video"]) {
    expect(transport).toContainElement(screen.getByRole("button", { name }));
  }
  expect(transport).toContainElement(screen.getByText("0:00 / 0:00"));

  for (const name of ["Rotate left", "Rotate right", "Loop: playlist", "Enter fullscreen"]) {
    expect(utilities).toContainElement(screen.getByRole("button", { name }));
  }
  expect(utilities).toContainElement(screen.getByRole("combobox", { name: "Playback speed" }));

  const play = screen.getByRole("button", { name: "Play" });
  expect(play).toHaveClass("play-button");
  expect(play.querySelector(".play-glyph")).toBeInTheDocument();
});

test("uses one control for playing and pausing", async () => {
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

  // Playing and pausing are the same action read through the current state, so
  // only ever one of the two is on screen.
  const transport = document.querySelector(".player-transport");
  expect(transport?.querySelectorAll(".play-button")).toHaveLength(1);
  expect(screen.queryByRole("button", { name: "Pause" })).not.toBeInTheDocument();

  fireEvent.play(video);
  const pause = screen.getByRole("button", { name: "Pause" });
  expect(pause).toHaveClass("play-button");
  expect(pause.querySelector(".pause-glyph")).toBeInTheDocument();
  expect(pause).toHaveAttribute("aria-keyshortcuts", "Space");
  expect(screen.queryByRole("button", { name: "Play" })).not.toBeInTheDocument();

  // The one button drives both directions, from the pointer and from Space.
  const paused = vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => {});
  await user.click(pause);
  expect(paused).toHaveBeenCalled();
  fireEvent.pause(video);
  expect(screen.getByRole("button", { name: "Play" })).toBeVisible();

  const played = vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
  fireEvent.keyDown(window, { key: " " });
  expect(played).toHaveBeenCalled();
  await waitFor(() => expect(screen.getByRole("button", { name: "Pause" })).toBeVisible());
});

test("starts the whole result page as a playlist positioned at the chosen video", async () => {
  const results = [1, 2, 3].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 3, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/playlist-2.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play playlist-2.mp4" }));

  // Choosing one result drops the viewer into the page's playlist at that spot
  // rather than playing it on its own.
  expect(await screen.findByLabelText("Playing playlist-2.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 2 of 3")).toBeVisible();
  expect(screen.getByRole("button", { name: "Playlist 3" })).toBeVisible();
  expect(screen.getAllByRole("listitem")).toHaveLength(3);
  expect(screen.getByRole("button", { name: "playlist-2.mp4" })).toHaveAttribute("aria-current", "true");

  // And the rest of the page is reachable from there in both directions.
  expect(screen.getByRole("button", { name: "Previous video" })).toBeEnabled();
  expect(screen.getByRole("button", { name: "Next video" })).toBeEnabled();
});

test("deletes the current video, advances, and restores it with the undo shortcut", async () => {
  const results = [1, 2, 3].map((number) => ({ id: `video-${number}`, fileName: `delete-${number}.mp4`, extension: "mp4" }));
  invokeMock.mockResolvedValueOnce({ query: "delete", page: 1, pageSize: 24, totalResults: 3, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/delete.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "delete{Enter}");
  await user.click(screen.getByRole("button", { name: "Play delete-2.mp4" }));
  expect(await screen.findByLabelText("Playing delete-2.mp4")).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Delete video" }));
  expect(await screen.findByLabelText("Playing delete-3.mp4")).toBeVisible();
  expect(invokeMock).toHaveBeenCalledWith("delete_video", { resultId: "video-2" });
  fireEvent.keyDown(window, { key: "Delete", shiftKey: true, ctrlKey: true });
  expect(await screen.findByLabelText("Playing delete-2.mp4")).toBeVisible();
  expect(invokeMock).toHaveBeenCalledWith("undo_delete");
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

  const track = document.querySelector("video")?.textTracks[0];
  expect(track?.label).toBe("Subtitles");
  expect(track?.mode).toBe("showing");
  expect((track?.cues?.[0] as VTTCue | undefined)?.text).toBe("Hi");

  await user.click(screen.getByRole("button", { name: "Subtitles" }));
  expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute("aria-pressed", "false");
  expect(track?.mode).toBe("disabled");
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
  const tracks = document.querySelector("video")?.textTracks;
  expect(tracks?.[tracks.length - 1]?.language).toBe("en");
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

test("shows its keyboard shortcut on every control that has one", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 2, totalPages: 1,
      results: [1, 2].map((n) => ({ id: `video-${n}`, fileName: `clip-${n}.mp4`, extension: "mp4" })),
    })
    .mockResolvedValue({ filePath: "/Videos/clip-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Video controls");

  const player = screen.getByLabelText(/^Player for /);
  const controls = [...player.querySelectorAll("[aria-keyshortcuts]")];
  expect(controls.length).toBeGreaterThan(10);
  for (const control of controls) {
    // A select cannot hold a child, so its hint sits beside it.
    const hint = control.querySelector(".key-hint") ?? control.parentElement?.querySelector(".key-hint");
    expect(hint, `${control.getAttribute("aria-label")} has no visible shortcut`).toBeTruthy();
  }

  // The example from the issue: 'rotate right' is bound to ']' and says so.
  const rotateRight = screen.getByRole("button", { name: "Rotate right" });
  expect(rotateRight).toHaveAttribute("aria-keyshortcuts", "]");
  expect(rotateRight.querySelector(".key-hint")).toHaveTextContent("]");

  // Hints read as key names rather than raw DOM ones.
  expect(
    screen.getByRole("button", { name: "Next video" }).querySelector(".key-hint"),
  ).toHaveTextContent("PgDn");
  expect(
    screen.getByRole("button", { name: "Previous video" }).querySelector(".key-hint"),
  ).toHaveTextContent("PgUp");
  expect(
    screen.getByRole("button", { name: "Back to results" }).querySelector(".key-hint"),
  ).toHaveTextContent("Esc");
});

test("steps playback speed with the keyboard", async () => {
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
  const speed = screen.getByRole("combobox", { name: "Playback speed" });

  fireEvent.keyDown(window, { key: "=" });
  expect(speed).toHaveValue("1.25");
  expect(video).toHaveProperty("playbackRate", 1.25);

  fireEvent.keyDown(window, { key: "-" });
  fireEvent.keyDown(window, { key: "-" });
  expect(speed).toHaveValue("0.75");

  // The ends of the range hold rather than wrapping around.
  fireEvent.keyDown(window, { key: "-" });
  fireEvent.keyDown(window, { key: "-" });
  expect(speed).toHaveValue("0.25");
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

test("shows fullscreen path and time overlays and toggles them with I", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen();
    await enterFullscreen(user);

    expect(screen.getByRole("region", { name: "Fullscreen video information" })).toBeVisible();
    expect(screen.getByText("/Videos/clip.mp4")).toBeVisible();
    expect(screen.getByRole("region", { name: "Fullscreen video information" })).toHaveTextContent("0:00 / 0:00");
    expect(screen.getByRole("button", { name: "Hide fullscreen information" })).toHaveAttribute("aria-keyshortcuts", "I");

    fireEvent.keyDown(window, { key: "i" });
    expect(screen.queryByRole("region", { name: "Fullscreen video information" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show fullscreen information" })).toBeVisible();
  } finally {
    vi.useRealTimers();
  }
});

test("uses Tauri window fullscreen when the web fullscreen request fails", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const requestFullscreen = vi.fn().mockRejectedValue(new Error("Web fullscreen failed"));
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", { configurable: true, value: requestFullscreen });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  await user.click(await screen.findByRole("button", { name: "Enter fullscreen" }));

  await waitFor(() => expect(windowApiMock.setFullscreen).toHaveBeenCalledWith(true));
  expect(screen.queryByRole("heading", { name: "This video could not be played" })).not.toBeInTheDocument();
  expect(screen.getByLabelText("Playing clip.mp4")).toBeVisible();
  expect(screen.getByRole("button", { name: "Exit fullscreen" })).toBeVisible();
});

test("uses Tauri window fullscreen first on macOS", async () => {
  const userAgent = vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue("AppleWebKit Mac OS X");
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

  try {
    await user.type(screen.getByRole("searchbox"), "clip{Enter}");
    await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
    await user.click(await screen.findByRole("button", { name: "Enter fullscreen" }));

    await waitFor(() => expect(windowApiMock.setFullscreen).toHaveBeenCalledWith(true));
    expect(requestFullscreen).not.toHaveBeenCalled();
    expect(screen.queryByRole("heading", { name: "This video could not be played" })).not.toBeInTheDocument();
    expect(screen.getByLabelText("Playing clip.mp4")).toBeVisible();
  } finally {
    userAgent.mockRestore();
  }
});

test("keeps player keyboard shortcuts live after leaving fullscreen from the keyboard", async () => {
  const userAgent = vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue("AppleWebKit Mac OS X");
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "clip{Enter}");
    await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
    const video = await screen.findByLabelText("Playing clip.mp4");

    fireEvent.keyDown(window, { key: "f" });
    await screen.findByRole("button", { name: "Exit fullscreen" });
    fireEvent.keyDown(window, { key: "f" });
    await screen.findByRole("button", { name: "Enter fullscreen" });

    expect(document.activeElement).toHaveClass("player-shell");
    fireEvent.keyDown(document.activeElement ?? window, { key: "]" });
    expect(video).toHaveStyle({ transform: "rotate(90deg)" });
  } finally {
    userAgent.mockRestore();
  }
});

// jsdom never actually goes fullscreen, so these stand in for the browser
// telling the player whether the request took effect.
function reportFullscreen(active: boolean) {
  Object.defineProperty(document, "fullscreenElement", {
    configurable: true,
    get: () => (active ? document.querySelector(".player-shell") : null),
  });
  act(() => void fireEvent(document, new Event("fullscreenchange")));
}

async function enterFullscreen(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "Enter fullscreen" }));
  reportFullscreen(true);
}

// The three fullscreen behaviours all need a player, fake timers and an engine
// that says yes to a fullscreen request, so they share one setup.
async function playForFullscreen(names = ["clip.mp4"]) {
  const results = names.map((fileName, position) => ({
    id: `video-${position + 1}`,
    fileName,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: results.length, totalPages: 1, results,
    })
    .mockResolvedValue({ filePath: `/Videos/${names[0]}` });
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: vi.fn().mockResolvedValue(undefined),
  });
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: names.length > 1 ? "Play all" : `Play ${names[0]}` }));
  return user;
}

// Moving the pointer is what wakes the overlay, and how far to the right it has
// come is what summons the playlist.
function movePointer(clientX = 10) {
  act(() => void fireEvent.mouseMove(window, { clientX, clientY: 10 }));
}

test("clears the whole overlay the moment fullscreen starts, leaving the scrubber", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen();
    const controls = await screen.findByLabelText("Video controls");
    expect(controls).not.toHaveClass("idle");
    expect(screen.getByRole("complementary", { name: "Playlist" })).toBeVisible();

    await enterFullscreen(user);

    // Straight away, not after the idle delay: fullscreen is for watching.
    expect(controls).toHaveClass("idle");
    expect(screen.queryByRole("complementary", { name: "Playlist" })).not.toBeInTheDocument();
    // Everything except the scrubber goes; it is the only feedback left about
    // how far into the video the viewer is.
    expect(screen.getByLabelText("Video timeline")).toBeInTheDocument();
    expect(controls).toContainElement(screen.getByLabelText("Video timeline"));

    // Coming back out restores the windowed player as it was left.
    reportFullscreen(false);
    expect(controls).not.toHaveClass("idle");
    expect(screen.getByRole("complementary", { name: "Playlist" })).toBeVisible();
  } finally {
    reportFullscreen(false);
    vi.useRealTimers();
  }
});

test("lets the fullscreen controls fade out and brings them back on movement", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen();
    const controls = await screen.findByLabelText("Video controls");
    expect(controls).not.toHaveClass("idle");

    await enterFullscreen(user);
    expect(controls).toHaveClass("idle");

    movePointer();
    expect(controls).not.toHaveClass("idle");

    act(() => void vi.advanceTimersByTime(3_000));
    expect(controls).toHaveClass("idle");

    movePointer();
    expect(controls).not.toHaveClass("idle");

    act(() => void vi.advanceTimersByTime(3_000));
    expect(controls).toHaveClass("idle");
    act(() => void fireEvent.mouseMove(document, { clientX: 10, clientY: 10 }));
    expect(controls).not.toHaveClass("idle");

    // Leaving fullscreen has to restore them for good.
    act(() => void vi.advanceTimersByTime(3_000));
    expect(controls).toHaveClass("idle");
    reportFullscreen(false);
    expect(controls).not.toHaveClass("idle");
    act(() => void vi.advanceTimersByTime(3_000));
    expect(controls).not.toHaveClass("idle");
  } finally {
    reportFullscreen(false);
    vi.useRealTimers();
  }
});

test("keeps the fullscreen controls up while the pointer rests on them", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen();
    const controls = await screen.findByLabelText("Video controls");
    await enterFullscreen(user);

    movePointer();
    act(() => void fireEvent.mouseEnter(controls));
    act(() => void vi.advanceTimersByTime(3_000));
    expect(controls).not.toHaveClass("idle");

    act(() => void fireEvent.mouseLeave(controls));
    act(() => void vi.advanceTimersByTime(3_000));
    expect(controls).toHaveClass("idle");
  } finally {
    reportFullscreen(false);
    vi.useRealTimers();
  }
});

const playlistDrawer = () => screen.queryByRole("complementary", { name: "Playlist" });

test("summons the fullscreen playlist from the right edge and dismisses it again", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen(["clip-1.mp4", "clip-2.mp4"]);
    await enterFullscreen(user);
    expect(playlistDrawer()).not.toBeInTheDocument();

    // Anywhere else on the picture wakes the controls without the playlist.
    movePointer(Math.round(window.innerWidth / 2));
    expect(playlistDrawer()).not.toBeInTheDocument();

    movePointer(window.innerWidth - 1);
    const drawer = playlistDrawer();
    expect(drawer).toBeVisible();

    // It stays for as long as the pointer is on it, however long that is.
    act(() => void fireEvent.mouseEnter(drawer as HTMLElement));
    act(() => void vi.advanceTimersByTime(5_000));
    expect(playlistDrawer()).toBeVisible();

    act(() => void fireEvent.mouseLeave(drawer as HTMLElement));
    act(() => void vi.advanceTimersByTime(1_000));
    expect(playlistDrawer()).not.toBeInTheDocument();
  } finally {
    reportFullscreen(false);
    vi.useRealTimers();
  }
});

test("holds the fullscreen playlist open from the keyboard until the pointer leaves it", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen(["clip-1.mp4", "clip-2.mp4"]);
    await enterFullscreen(user);
    expect(playlistDrawer()).not.toBeInTheDocument();

    // The heading's toggle is out of reach in fullscreen, so the overlay carries
    // the same control and the same shortcut.
    expect(screen.getByRole("button", { name: "Playlist" })).toHaveAttribute("aria-keyshortcuts", "P");

    act(() => void fireEvent.keyDown(window, { key: "p" }));
    const drawer = playlistDrawer();
    expect(drawer).toBeVisible();

    // A drawer nobody pointed at is not taken away underneath them.
    act(() => void vi.advanceTimersByTime(5_000));
    expect(playlistDrawer()).toBeVisible();

    act(() => void fireEvent.mouseEnter(drawer as HTMLElement));
    act(() => void fireEvent.mouseLeave(drawer as HTMLElement));
    act(() => void vi.advanceTimersByTime(1_000));
    expect(playlistDrawer()).not.toBeInTheDocument();
  } finally {
    reportFullscreen(false);
    vi.useRealTimers();
  }
});

test("leaves windowed playback with its permanent playlist and its controls up", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    await playForFullscreen(["clip-1.mp4", "clip-2.mp4"]);
    const controls = await screen.findByLabelText("Video controls");

    // None of the fullscreen rules run windowed: the drawer stays put, the
    // right-hand edge does nothing, and the overlay never fades.
    expect(playlistDrawer()).toBeVisible();
    movePointer(window.innerWidth - 1);
    act(() => void vi.advanceTimersByTime(5_000));
    expect(playlistDrawer()).toBeVisible();
    expect(controls).not.toHaveClass("idle");
  } finally {
    vi.useRealTimers();
  }
});

// A three-state control cannot say which state it is in with `aria-pressed`,
// so its name is what the tests read.
function loopControl() {
  return screen.getByRole("button", { name: /^Loop: / });
}

test("cycles the loop control through the playlist, one video and off", async () => {
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
  await screen.findByLabelText("Video controls");

  // Looping the whole playlist is the default, so a sitting keeps going.
  expect(loopControl()).toHaveAccessibleName("Loop: playlist");
  expect(loopControl()).toHaveClass("loop-button", "on");
  expect(loopControl().querySelector(".loop-one")).not.toBeInTheDocument();

  await user.click(loopControl());
  expect(loopControl()).toHaveAccessibleName("Loop: this video");
  expect(loopControl()).toHaveClass("on");
  // VLC's marker for repeating one video: a "1" drawn between the arrows.
  expect(loopControl().querySelector(".loop-one")).toBeInTheDocument();

  await user.click(loopControl());
  expect(loopControl()).toHaveAccessibleName("Loop: off");
  expect(loopControl()).not.toHaveClass("on");

  await user.click(loopControl());
  expect(loopControl()).toHaveAccessibleName("Loop: playlist");

  // The keyboard cycles the same way.
  fireEvent.keyDown(window, { key: "l" });
  expect(loopControl()).toHaveAccessibleName("Loop: this video");
  expect(loopControl()).toHaveAttribute("aria-keyshortcuts", "L");
});

test("replays a one-entry playlist rather than stopping at its end", async () => {
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

  // Looping a playlist of one means playing that one again; wrapping to the
  // index it is already on is a state change React would throw away.
  const play = vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
  play.mockClear();
  fireEvent.ended(video);
  expect(play).toHaveBeenCalled();
  expect(screen.getByLabelText("Playing clip.mp4")).toBeVisible();
});

test("repeats the current video in loop-one mode without leaving the playlist", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  const video = await screen.findByLabelText("Playing playlist-1.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  fireEvent.loadedMetadata(video);

  await user.click(loopControl());
  const play = vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
  play.mockClear();
  fireEvent.ended(video);

  // The first entry starts again rather than the playlist moving on.
  expect(play).toHaveBeenCalled();
  expect(screen.getByLabelText("Playing playlist-1.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 1 of 2")).toBeVisible();
  expect(screen.getByLabelText("Video timeline")).toHaveValue("0");
});

test("stops instead of advancing when looping is off", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  const video = await screen.findByLabelText("Playing playlist-1.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  fireEvent.loadedMetadata(video);

  await user.click(loopControl());
  await user.click(loopControl());
  expect(loopControl()).toHaveAccessibleName("Loop: off");

  const play = vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
  play.mockClear();
  fireEvent.ended(video);

  // "Off" means this one video and then nothing, even in the middle of a list.
  expect(screen.getByLabelText("Playing playlist-1.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 1 of 2")).toBeVisible();
  expect(play).not.toHaveBeenCalled();
});

test("fills the scrubber to the whole duration when a video ends", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  const video = await screen.findByLabelText("Playing playlist-1.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  fireEvent.loadedMetadata(video);

  // The engine stops reporting time a little short of the end, which left the
  // bar visibly unfinished and made every video look as though it had been cut.
  Object.defineProperty(video, "currentTime", { configurable: true, writable: true, value: 112 });
  fireEvent.timeUpdate(video);
  const timeline = screen.getByLabelText("Video timeline");
  expect(timeline).toHaveValue("112");

  await user.click(loopControl());
  await user.click(loopControl());
  fireEvent.ended(video);
  expect(timeline).toHaveValue("120");
  expect(screen.getByText("2:00 / 2:00")).toBeVisible();
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

test("offers tooltips for every actionable result and player control", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 2, totalPages: 2,
      results: [
        { id: "video-1", fileName: "clip.mp4", extension: "mp4" },
        { id: "video-2", fileName: "other.mp4", extension: "mp4" },
      ],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  expect(screen.getByRole("button", { name: "Clear search" })).toHaveAttribute("title", "Clear search");
  expect(screen.getByRole("button", { name: "Play all" })).toHaveAttribute("title", "Play all videos");
  expect(screen.getByRole("button", { name: "Play clip.mp4" })).toHaveAttribute("title", "Play clip.mp4");
  expect(screen.getByText("2 of 2 loaded")).toBeVisible();

  await user.click(screen.getByRole("button", { name: "Play clip.mp4" }));
  const controls = await screen.findByLabelText("Video controls");
  for (const control of controls.querySelectorAll<HTMLButtonElement | HTMLSelectElement>("button, select")) {
    expect(control, `${control.getAttribute("aria-label")} has no tooltip`).toHaveAttribute("title");
    expect(control.title).not.toBe("");
  }
  expect(screen.getByRole("button", { name: "Back to results" })).toHaveAttribute("title", "Back to results");
  expect(screen.getByRole("button", { name: "clip.mp4" })).toHaveAttribute("title", "Play clip.mp4");
});

test("supports playback speeds from 0.1x through 4x", async () => {
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
  const speed = screen.getByRole("combobox", { name: "Playback speed" });

  expect(speed).toHaveValue("1");
  expect(speed.querySelector("option:first-child")).toHaveValue("0.1");
  expect(speed.querySelector("option:last-child")).toHaveValue("4");
  await user.selectOptions(speed, "0.1");
  expect(speed).toHaveValue("0.1");
  fireEvent.keyDown(window, { key: "=" });
  expect(speed).toHaveValue("0.25");
  await user.selectOptions(speed, "4");
  expect(speed).toHaveValue("4");
  fireEvent.keyDown(window, { key: "-" });
  expect(speed).toHaveValue("3");
});

test("skips web playback by ten seconds", async () => {
  invokeMock.mockResolvedValueOnce({ query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1, results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }] }).mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup(); render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}"); await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  const video = await screen.findByLabelText("Playing clip.mp4"); Object.defineProperty(video, "duration", { configurable: true, value: 120 }); Object.defineProperty(video, "currentTime", { configurable: true, writable: true, value: 20 });
  fireEvent.timeUpdate(video); await user.click(screen.getByRole("button", { name: "Skip forward 10 seconds" })); expect((video as HTMLVideoElement).currentTime).toBe(30);
});

test("uses left and right arrows for ten-second seeking", async () => {
  invokeMock.mockResolvedValueOnce({
    query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
    results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
  }).mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  const video = await screen.findByLabelText("Playing clip.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  Object.defineProperty(video, "currentTime", { configurable: true, writable: true, value: 20 });
  fireEvent.timeUpdate(video);

  fireEvent.keyDown(window, { key: "ArrowLeft" });
  expect(video).toHaveProperty("currentTime", 10);
  fireEvent.keyDown(window, { key: "ArrowRight" });
  expect(video).toHaveProperty("currentTime", 20);
  expect(screen.getByRole("button", { name: "Skip back 10 seconds" })).toHaveAttribute("aria-keyshortcuts", "ArrowLeft");
  expect(screen.getByRole("button", { name: "Skip forward 10 seconds" })).toHaveAttribute("aria-keyshortcuts", "ArrowRight");
});

test("changes volume with the discoverable keyboard shortcuts", async () => {
  invokeMock.mockResolvedValueOnce({
    query: "clip", page: 1, pageSize: 24, totalResults: 1, totalPages: 1,
    results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
  }).mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play clip.mp4" }));
  await screen.findByLabelText("Playing clip.mp4");

  const volume = screen.getByRole("slider", { name: "Volume" });
  expect(volume).toHaveValue("100");
  fireEvent.keyDown(window, { key: "9" });
  expect(volume).toHaveValue("95");
  expect(screen.getByRole("button", { name: "Decrease volume" })).toHaveAttribute("aria-keyshortcuts", "9");
  expect(screen.getByRole("button", { name: "Increase volume" })).toHaveAttribute("aria-keyshortcuts", "0");
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
  fireEvent.keyDown(window, { key: "PageDown" });
  expect(await screen.findByLabelText("Playing clip-2.mp4")).toBeVisible();
  fireEvent.keyDown(window, { key: "PageUp" });
  expect(await screen.findByLabelText("Playing clip-1.mp4")).toBeVisible();
  fireEvent.keyDown(window, { key: "l" });
  expect(loopControl()).toHaveAccessibleName("Loop: this video");
  fireEvent.keyDown(window, { key: "f" });
  expect(requestFullscreen).toHaveBeenCalledOnce();

  search.focus();
  fireEvent.keyDown(search, { key: "]" });
  expect(screen.getByLabelText("Playing clip-1.mp4")).toHaveStyle({ transform: "rotate(0deg)" });
});

test("keeps keyboard shortcuts active after a transport button receives focus", async () => {
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
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  Object.defineProperty(video, "currentTime", { configurable: true, writable: true, value: 20 });
  fireEvent.timeUpdate(video);

  const skipBack = screen.getByRole("button", { name: "Skip back 10 seconds" });
  await user.click(skipBack);
  expect(skipBack).toHaveFocus();
  fireEvent.keyDown(skipBack, { key: " " });

  expect(video).toHaveProperty("paused", true);
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
  // Looping the playlist is the default, so nothing has to be turned on first.
  expect(loopControl()).toHaveAccessibleName("Loop: playlist");
  fireEvent.ended(await screen.findByLabelText("Playing playlist-1.mp4"));
  fireEvent.ended(await screen.findByLabelText("Playing playlist-2.mp4"));
  expect(await screen.findByLabelText("Playing playlist-1.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 1 of 2")).toBeVisible();
});

test("loads more results as the infinite list reaches its end and reports provider failures", async () => {
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
  vi.stubGlobal("IntersectionObserver", class {
    private readonly callback: IntersectionObserverCallback;
    constructor(callback: IntersectionObserverCallback) { this.callback = callback; }
    observe(target: Element) {
      if (target.classList.contains("load-more-marker")) {
        this.callback([{ isIntersecting: true } as IntersectionObserverEntry], this as unknown as IntersectionObserver);
      }
    }
    disconnect() {}
    unobserve() {}
    takeRecords() { return []; }
    root = null;
    rootMargin = "0px";
    thresholds = [];
  });
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  expect(await screen.findByRole("button", { name: "Play clip-24.mp4" })).toBeVisible();
  expect(screen.getByText("2 of 25 loaded")).toBeVisible();

  await user.clear(screen.getByRole("searchbox"));
  await user.type(screen.getByRole("searchbox"), "broken{Enter}");
  expect(await screen.findByRole("alert")).toHaveTextContent("Recoll search could not start.");
  await waitFor(() => expect(screen.getByRole("searchbox")).toHaveValue("broken"));
});

test("loads every search page into the playlist before playing all", async () => {
  const firstPage = Array.from({ length: 24 }, (_, index) => ({
    id: `video-${index + 1}`,
    fileName: `clip-${index + 1}.mp4`,
    extension: "mp4",
  }));
  const last = { id: "video-25", fileName: "clip-25.mp4", extension: "mp4" };
  invokeMock
    .mockResolvedValueOnce({
      query: "clip", page: 1, pageSize: 24, totalResults: 25, totalPages: 2,
      results: firstPage,
    })
    .mockResolvedValueOnce({
      query: "clip", page: 2, pageSize: 24, totalResults: 25, totalPages: 2,
      results: [last],
    })
    .mockResolvedValue({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  const playlist = await screen.findByRole("complementary", { name: "Playlist" });
  expect(playlist.querySelectorAll("li")).toHaveLength(25);
  expect(screen.getByText("Playlist video 1 of 25")).toBeVisible();
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
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-3.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" });
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

  // The end of the last entry wraps round to the first rather than stopping.
  expect(await screen.findByLabelText("Playing playlist-1.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 1 of 3")).toBeVisible();
  expect(invokeMock).toHaveBeenLastCalledWith("prepare_video", { resultId: "video-1" });
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

test("keeps the chosen playback speed when the playlist advances", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-2.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  const first = await screen.findByLabelText("Playing playlist-1.mp4");
  await user.selectOptions(screen.getByRole("combobox", { name: "Playback speed" }), "1.5");
  expect(first).toHaveProperty("playbackRate", 1.5);

  fireEvent.ended(first);
  const second = await screen.findByLabelText("Playing playlist-2.mp4");
  fireEvent.loadedMetadata(second);

  // Choosing a speed is a decision about the sitting, not about one file.
  expect(screen.getByRole("combobox", { name: "Playback speed" })).toHaveValue("1.5");
  expect(second).toHaveProperty("playbackRate", 1.5);
});

test("re-applies the chosen playback speed to the next native video", async () => {
  const results = [1, 2].map((number) => ({
    id: `native-${number}`,
    fileName: `native-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      return Promise.resolve({ query: "native", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results });
    }
    if (command === "prepare_video") {
      const resultId = (args as { resultId: string }).resultId;
      return Promise.resolve({ filePath: `/Videos/${resultId}.mp4`, playbackBackend: "native" });
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
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Playing native-1.mp4");
  await user.selectOptions(screen.getByRole("combobox", { name: "Playback speed" }), "1.5");
  await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("set_native_speed", { speed: 1.5 }));

  invokeMock.mockClear();
  await user.click(screen.getByRole("button", { name: "Next video" }));
  await screen.findByLabelText("Playing native-2.mp4");

  // mpv starts every file at 1x, so the retained speed has to be sent again.
  await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("set_native_speed", { speed: 1.5 }));
  expect(screen.getByRole("combobox", { name: "Playback speed" })).toHaveValue("1.5");
});

test("stays fullscreen when the playlist advances to the next video", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-2.mp4" });
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: vi.fn().mockResolvedValue(undefined),
  });
  const user = userEvent.setup();
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
    await user.click(await screen.findByRole("button", { name: "Play all" }));
    const first = await screen.findByLabelText("Playing playlist-1.mp4");
    await enterFullscreen(user);
    const shell = document.querySelector(".player-shell");
    expect(screen.getByRole("button", { name: "Exit fullscreen" })).toBeVisible();

    fireEvent.ended(first);
    await screen.findByLabelText("Playing playlist-2.mp4");

    // The browser drops out of fullscreen the moment the element it promoted
    // leaves the document, so the shell has to outlive the video inside it.
    expect(document.querySelector(".player-shell")).toBe(shell);
    expect(screen.getByRole("button", { name: "Exit fullscreen" })).toBeVisible();
  } finally {
    reportFullscreen(false);
  }
});

test("keeps fullscreen controls hidden when autoplay advances the playlist", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen(["clip-1.mp4", "clip-2.mp4"]);
    await enterFullscreen(user);
    const controls = await screen.findByLabelText("Video controls");
    act(() => void vi.advanceTimersByTime(3_000));
    expect(controls).toHaveClass("idle");

    fireEvent.ended(await screen.findByLabelText("Playing clip-1.mp4"));
    await screen.findByLabelText("Playing clip-2.mp4");
    expect(controls).toHaveClass("idle");
  } finally {
    reportFullscreen(false);
    vi.useRealTimers();
  }
});

test("keeps the native video surface behind the playlist drawer", async () => {
  const results = [1, 2].map((number) => ({
    id: `native-${number}`,
    fileName: `native-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      return Promise.resolve({ query: "native", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results });
    }
    if (command === "prepare_video") {
      const resultId = (args as { resultId: string }).resultId;
      return Promise.resolve({ filePath: `/Videos/${resultId}.mp4`, playbackBackend: "native" });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({ duration: 120, currentTime: 1, paused: false, ended: false });
    }
    return Promise.resolve();
  });
  // jsdom has no layout, so the player is handed the geometry a real window
  // would have: an 800x400 picture, the overlay along the bottom 80px and the
  // drawer down the right-hand 240px.
  const layout: Record<string, Partial<DOMRect>> = {
    "native-video": { x: 0, y: 0, top: 0, left: 0, right: 800, bottom: 400, width: 800, height: 400 },
    "player-controls": { x: 0, y: 320, top: 320, left: 0, right: 800, bottom: 400, width: 800, height: 80 },
    "playlist-drawer": { x: 560, y: 0, top: 0, left: 560, right: 800, bottom: 400, width: 240, height: 400 },
  };
  const boxes = vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    const named = Object.keys(layout).find((name) => this.classList.contains(name));
    const empty = { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
    return { ...empty, ...(named ? layout[named] : {}) } as DOMRect;
  });
  const user = userEvent.setup();
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "native{Enter}");
    await user.click(await screen.findByRole("button", { name: "Play all" }));
    await screen.findByLabelText("Playing native-1.mp4");

    expect(screen.getByRole("button", { name: "Playlist 2" })).toBeVisible();
  } finally {
    boxes.mockRestore();
  }
});

test("hands the fullscreen native surface everything but the scrubber sliver", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const results = [1, 2].map((number) => ({
    id: `native-${number}`,
    fileName: `native-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      return Promise.resolve({ query: "native", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results });
    }
    if (command === "prepare_video") {
      const resultId = (args as { resultId: string }).resultId;
      return Promise.resolve({ filePath: `/Videos/${resultId}.mp4`, playbackBackend: "native" });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({ duration: 120, currentTime: 1, paused: false, ended: false });
    }
    return Promise.resolve();
  });
  // jsdom has no layout, so a 1200x800 fullscreen window is described here: the
  // stylesheet keeps the picture 6px clear of the bottom for the scrubber, the
  // collapsed overlay is exactly that sliver, and the full overlay is 96px tall.
  const box = (left: number, top: number, width: number, height: number) =>
    ({ x: left, y: top, top, left, right: left + width, bottom: top + height, width, height }) as DOMRect;
  const boxes = vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    if (this.classList.contains("native-video")) return box(0, 0, 1200, 794);
    if (this.classList.contains("player-controls")) {
      return this.classList.contains("idle") ? box(0, 794, 1200, 6) : box(0, 704, 1200, 96);
    }
    if (this.classList.contains("playlist-drawer")) return box(920, 0, 280, 800);
    return box(0, 0, 0, 0);
  });
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: vi.fn().mockResolvedValue(undefined),
  });
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "native{Enter}");
    await user.click(await screen.findByRole("button", { name: "Play all" }));
    await screen.findByLabelText("Playing native-1.mp4");
    await enterFullscreen(user);

    // Revealing controls or the drawer never changes the native surface
    // bounds, so mpv does not resize or re-center the picture. The end-to-end
    // Linux test verifies those native bounds against a real GL surface.
    movePointer();
    movePointer(window.innerWidth - 1);
    expect(screen.getByRole("button", { name: "Exit fullscreen" })).toBeVisible();
  } finally {
    reportFullscreen(false);
    boxes.mockRestore();
    vi.useRealTimers();
  }
});

test("draws the heading controls without font-dependent glyphs", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  // A bare "←" is substituted from whichever font the system has for that
  // codepoint, which lands off the label's baseline on Linux.
  const back = await screen.findByRole("button", { name: "Back to results" });
  expect(back.querySelector("svg.back-arrow")).toBeInTheDocument();
  expect(back).not.toHaveTextContent("←");
  expect(back).toHaveTextContent("Back");

  for (const name of ["Previous video", "Next video"]) {
    expect(screen.getByRole("button", { name }).querySelector("svg.control-icon")).toBeInTheDocument();
  }
});

test("wraps playlist navigation at both ends", async () => {
  const results = [1, 2, 3].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 3, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/playlist.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Playing playlist-1.mp4");

  const previous = () => screen.getByRole("button", { name: "Previous video" });
  const next = () => screen.getByRole("button", { name: "Next video" });
  expect(previous()).toBeEnabled();
  expect(next()).toBeEnabled();

  // In the middle of a playlist both directions are available.
  fireEvent.keyDown(window, { key: "PageDown" });
  await screen.findByLabelText("Playing playlist-2.mp4");
  expect(previous()).toBeEnabled();
  expect(next()).toBeEnabled();

  fireEvent.keyDown(window, { key: "PageDown" });
  await screen.findByLabelText("Playing playlist-3.mp4");
  expect(previous()).toBeEnabled();
  expect(next()).toBeEnabled();

  fireEvent.keyDown(window, { key: "PageDown" });
  expect(await screen.findByLabelText("Playing playlist-1.mp4")).toBeVisible();

  fireEvent.keyDown(window, { key: "PageUp" });
  expect(await screen.findByLabelText("Playing playlist-3.mp4")).toBeVisible();
});

test("wraps every button label so the stylesheet can trim it", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({ query: "playlist", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results })
    .mockResolvedValue({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Video controls");

  // A loose run of text in a flex row is an anonymous box the stylesheet cannot
  // reach, so its line box — descender space and all — is what gets centred,
  // and the label reads a few pixels above the icon beside it.
  const loose = [...document.querySelectorAll<HTMLElement>(".player-controls button, .back-button, .playlist-toggle")]
    .filter((control) =>
      [...control.childNodes].some((node) => node.nodeType === Node.TEXT_NODE && node.textContent?.trim()),
    )
    .map((control) => control.getAttribute("aria-label") ?? control.textContent);
  expect(loose).toEqual([]);

  for (const name of ["Back to results", "Skip back 10 seconds", "Skip forward 10 seconds", "Subtitles"]) {
    expect(screen.getByRole("button", { name }).querySelector(".control-label")).toBeInTheDocument();
  }
  expect(screen.getByRole("button", { name: "Playlist 2" }).querySelector(".playlist-count .control-label"))
    .toHaveTextContent("2");
});

test("native playlist advances when libmpv reports end of file", async () => {
  const results = [1, 2].map((number) => ({
    id: `native-${number}`,
    fileName: `native-${number}.mp4`,
    extension: "mp4",
  }));
  let loaded = "";
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
    if (command === "load_native_video") {
      loaded = String((args as { filePath: string }).filePath);
      return Promise.resolve();
    }
    if (command === "native_playback_state") {
      // Only the first file has run out, so the playlist has somewhere to go
      // and the test is not racing a second advance.
      const ended = loaded.includes("native-1");
      return Promise.resolve({ duration: 30, currentTime: ended ? 28 : 1, paused: ended, ended });
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

test("runs the native scrubber to the end and honours loop off", async () => {
  const results = [1, 2].map((number) => ({
    id: `native-${number}`,
    fileName: `native-${number}.mp4`,
    extension: "mp4",
  }));
  let ended = false;
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      return Promise.resolve({ query: "native", page: 1, pageSize: 24, totalResults: 2, totalPages: 1, results });
    }
    if (command === "prepare_video") {
      const resultId = (args as { resultId: string }).resultId;
      return Promise.resolve({ filePath: `/Videos/${resultId}.mp4`, playbackBackend: "native" });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      // mpv's last reported position falls short of the duration, which is what
      // stopped the scrubber before the end of the bar.
      return Promise.resolve({ duration: 30, currentTime: ended ? 28 : 4, paused: ended, ended });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Playing native-1.mp4");
  await waitFor(() => expect(screen.getByLabelText("Video timeline")).toHaveValue("4"));

  await user.click(loopControl());
  await user.click(loopControl());
  expect(loopControl()).toHaveAccessibleName("Loop: off");

  ended = true;
  await waitFor(() => expect(screen.getByLabelText("Video timeline")).toHaveValue("30"), { timeout: 2_000 });
  expect(screen.getByText("0:30 / 0:30")).toBeVisible();
  // "Off" holds at the end of this video rather than starting the next one.
  expect(screen.getByLabelText("Playing native-1.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 1 of 2")).toBeVisible();
});
