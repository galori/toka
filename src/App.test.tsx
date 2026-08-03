import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { externalPlayers, launchPlaylist } from "./api";
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

// Asked for once at startup each, on a screen most of these tests never reach.
// Left on the real `invoke` they would take the first queued responses out of
// every test's mock, so these calls are stubbed at the module seam instead;
// `openInExternalPlayer` stays real, so what Toka sends the backend is still
// asserted through `invoke`.
vi.mock("./api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./api")>()),
  externalPlayers: vi.fn(() => Promise.resolve([])),
  launchPlaylist: vi.fn(() => Promise.resolve(null)),
}));

const invokeMock = vi.mocked(invoke);
const externalPlayersMock = vi.mocked(externalPlayers);
const launchPlaylistMock = vi.mocked(launchPlaylist);
const convertFileSrcMock = vi.mocked(convertFileSrc);
const randomMock = vi.spyOn(Math, "random");

test("shuffles without mutating the original video list", () => {
  const videos = [1, 2, 3].map((id) => ({
    id: String(id),
    fileName: `${id}.mp4`,
    extension: "mp4",
  }));
  randomMock.mockReturnValueOnce(0).mockReturnValueOnce(0);
  expect(shuffleVideos(videos).map((video) => video.id)).toEqual([
    "2",
    "3",
    "1",
  ]);
  expect(videos.map((video) => video.id)).toEqual(["1", "2", "3"]);
});

test("shows build provenance on the initial home screen", () => {
  render(<App />);

  expect(
    screen.getByRole("region", { name: "Build information" }),
  ).toHaveTextContent("Version 0.1.0");
  expect(
    screen.getByRole("region", { name: "Build information" }),
  ).toHaveTextContent("Built");
  expect(
    screen.getByRole("region", { name: "Build information" }),
  ).toHaveTextContent("Git SHA");
});

beforeEach(() => {
  invokeMock.mockReset();
  externalPlayersMock.mockReset().mockResolvedValue([]);
  launchPlaylistMock.mockReset().mockResolvedValue(null);
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
  vi.stubEnv("VITE_E2E_FIXTURE_SERVER_PORT", "23142");
  const userAgent = vi
    .spyOn(window.navigator, "userAgent", "get")
    .mockReturnValue("WebKitGTK Linux");

  try {
    expect(playbackSource("/Videos/clip #1.mp4")).toBe(
      "http://127.0.0.1:23142/clip%20%231.mp4",
    );
    expect(convertFileSrcMock).not.toHaveBeenCalled();
  } finally {
    userAgent.mockRestore();
  }
});

test("retains the asset protocol for E2E builds on platforms that support it", () => {
  vi.stubEnv("VITE_E2E", "1");
  const userAgent = vi
    .spyOn(window.navigator, "userAgent", "get")
    .mockReturnValue("AppleWebKit Mac OS X");

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
    results: [
      { id: "video-1", fileName: "Summer Vacation.mp4", extension: "mp4" },
    ],
  });
  const user = userEvent.setup();
  render(<App />);

  const search = screen.getByRole("searchbox", { name: "Search videos" });
  expect(search).toHaveFocus();
  expect(
    screen.queryByRole("list", { name: "Video results" }),
  ).not.toBeInTheDocument();

  await user.type(search, "summer vacation{Enter}");

  expect(
    await screen.findByRole("button", { name: "Play Summer Vacation.mp4" }),
  ).toBeVisible();
  expect(invokeMock).toHaveBeenCalledWith("search_videos", {
    request: {
      query: "summer vacation",
      page: 1,
      pageSize: 24,
      fields: { tags: true, fileName: true, path: false },
    },
  });
});

const searchPage = (query: string) => ({
  query,
  page: 1,
  pageSize: 24,
  totalResults: 1,
  totalPages: 1,
  results: [{ id: "video-1", fileName: `${query}.mp4`, extension: "mp4" }],
});

const lastSearchFields = () => {
  const call = invokeMock.mock.calls
    .filter(([command]) => command === "search_videos")
    .at(-1);
  return (call?.[1] as { request: { fields: unknown } } | undefined)?.request
    .fields;
};

test("chooses which parts of a video a search looks at", async () => {
  invokeMock.mockResolvedValue(searchPage("holiday"));
  const user = userEvent.setup();
  render(<App />);

  const tags = screen.getByRole("button", { name: "Search tags" });
  const filename = screen.getByRole("button", { name: "Search filename" });
  const path = screen.getByRole("button", { name: "Search path" });
  expect(tags).toHaveAttribute("aria-keyshortcuts", "Ctrl+T");
  expect(filename).toHaveAttribute("aria-keyshortcuts", "Ctrl+F");
  expect(path).toHaveAttribute("aria-keyshortcuts", "Ctrl+P");
  expect(tags).toHaveAttribute("aria-pressed", "true");
  expect(filename).toHaveAttribute("aria-pressed", "true");
  expect(path).toHaveAttribute("aria-pressed", "false");
  // Nothing has been searched for yet, so a choice about the next search is
  // not a search of its own.
  await user.click(path);
  expect(invokeMock).not.toHaveBeenCalledWith(
    "search_videos",
    expect.anything(),
  );

  await user.type(screen.getByRole("searchbox"), "holiday{Enter}");

  expect(
    await screen.findByRole("button", { name: "Play holiday.mp4" }),
  ).toBeVisible();
  expect(lastSearchFields()).toEqual({
    tags: true,
    fileName: true,
    path: true,
  });
});

// A long page of results puts the field far off the top of the screen, and
// clicking a tile or a tag hands the keyboard to whatever was clicked. Getting
// back to the field should not mean scrolling to it.
test("jumps to the search field from its keyboard shortcut", async () => {
  invokeMock.mockResolvedValue(searchPage("holiday"));
  const user = userEvent.setup();
  render(<App />);

  const field = screen.getByRole("searchbox");
  expect(field).toHaveAttribute("aria-keyshortcuts", "Ctrl+K");
  expect(field.parentElement?.querySelector(".key-hint")).toHaveTextContent(
    "Ctrl+K",
  );

  await user.type(field, "holiday{Enter}");
  const tile = await screen.findByRole("button", {
    name: "Play holiday.mp4",
  });
  tile.focus();
  expect(field).not.toHaveFocus();

  fireEvent.keyDown(tile, { key: "k", ctrlKey: true });
  expect(field).toHaveFocus();
  // Selected rather than merely focused, so the next thing typed asks a new
  // question instead of being appended to the old one.
  expect(field).toHaveValue("holiday");
  await user.keyboard("beach");
  expect(field).toHaveValue("beach");
});

test("searches again as soon as the parts being searched change", async () => {
  invokeMock.mockResolvedValue(searchPage("holiday"));
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "holiday{Enter}");
  expect(
    await screen.findByRole("button", { name: "Play holiday.mp4" }),
  ).toBeVisible();

  fireEvent.keyDown(window, { key: "f", ctrlKey: true });

  await waitFor(() =>
    expect(lastSearchFields()).toEqual({
      tags: true,
      fileName: false,
      path: false,
    }),
  );
  expect(
    screen.getByRole("button", { name: "Search filename" }),
  ).toHaveAttribute("aria-pressed", "false");
});

test("keeps the last part of a search selected", async () => {
  render(<App />);
  const tags = await screen.findByRole("button", { name: "Search tags" });

  fireEvent.keyDown(window, { key: "f", ctrlKey: true });

  // Tags are all that is left, and a search with nothing to match against can
  // only ever answer "no videos".
  expect(tags).toBeDisabled();
  fireEvent.keyDown(window, { key: "t", ctrlKey: true });
  expect(tags).toHaveAttribute("aria-pressed", "true");
});

test("offers a results shuffle control and restarts a shuffled playlist with R", async () => {
  const results = [1, 2, 3].map((number) => ({
    id: `video-${number}`,
    fileName: `shuffle-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "shuffle",
      page: 1,
      pageSize: 24,
      totalResults: 3,
      totalPages: 1,
      results,
    })
    .mockResolvedValue({ filePath: "/Videos/shuffle.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "shuffle{Enter}");
  const resultsButton = await screen.findByRole("button", {
    name: "Shuffle results",
  });
  expect(resultsButton).toHaveAttribute("aria-keyshortcuts", "R");
  await user.click(screen.getByRole("button", { name: "Play shuffle-2.mp4" }));
  expect(await screen.findByLabelText("Playing shuffle-2.mp4")).toBeVisible();
  expect(
    screen.getByRole("button", { name: "Shuffle playlist" }),
  ).toHaveAttribute("aria-keyshortcuts", "R");
  randomMock.mockReturnValueOnce(0.999999).mockReturnValueOnce(0.999999);
  fireEvent.keyDown(window, { key: "r" });
  expect(await screen.findByLabelText("Playing shuffle-1.mp4")).toBeVisible();
});

test("displays an app-generated thumbnail when search returns one", async () => {
  invokeMock.mockResolvedValueOnce({
    query: "clip",
    page: 1,
    pageSize: 24,
    totalResults: 1,
    totalPages: 1,
    results: [
      {
        id: "video-1",
        fileName: "clip.mp4",
        extension: "mp4",
        thumbnailPath: "/tmp/toka-thumbnails/clip.jpg",
      },
    ],
  });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");

  expect(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  ).toBeVisible();
  expect(
    screen
      .getByRole("button", { name: "Play clip.mp4" })
      .querySelector(".video-art"),
  ).toHaveStyle({
    backgroundImage: 'url("asset:///tmp/toka-thumbnails/clip.jpg")',
  });
});

test("shows video tags and edits them with the tag helper", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "tagged",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [
        {
          id: "video-1",
          fileName: "tagged.mp4",
          extension: "mp4",
          tags: ["work"],
        },
      ],
    })
    .mockResolvedValueOnce({
      fileName: "tagged [review work].mp4",
      tags: ["review", "work"],
    })
    .mockResolvedValueOnce({
      fileName: "tagged [review].mp4",
      tags: ["review"],
    });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "tagged{Enter}");
  expect(
    await screen.findByRole("button", { name: "Remove tag work" }),
  ).toBeVisible();
  await user.click(
    screen.getByRole("button", { name: "Add tag to tagged.mp4" }),
  );
  await user.type(
    screen.getByRole("textbox", { name: "Add tag to tagged.mp4" }),
    "review{Enter}",
  );
  expect(
    await screen.findByRole("button", { name: "Remove tag review" }),
  ).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Remove tag work" }));
  expect(invokeMock).toHaveBeenCalledWith("add_video_tags", {
    resultId: "video-1",
    tags: ["review"],
  });
  expect(invokeMock).toHaveBeenCalledWith("remove_video_tags", {
    resultId: "video-1",
    tags: ["work"],
  });
});

test("hands a multi-word tag entry over as the several tags it is", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "sample1",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [
        {
          id: "video-1",
          fileName: "sample1 [old].mp4",
          extension: "mp4",
          tags: ["old"],
        },
      ],
    })
    .mockResolvedValueOnce({
      fileName: "sample1 [house old vacation].mp4",
      tags: ["house", "old", "vacation"],
    });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "sample1{Enter}");
  expect(
    await screen.findByRole("button", { name: "Remove tag old" }),
  ).toBeVisible();
  await user.click(
    screen.getByRole("button", { name: "Add tag to sample1 [old].mp4" }),
  );
  await user.type(
    screen.getByRole("textbox", { name: "Add tag to sample1 [old].mp4" }),
    "  vacation house  {Enter}",
  );

  // The entry travels whole. Splitting it on whitespace, lowercasing, sorting
  // and dropping the words the name already carries is the tag layer's job, so
  // every caller of the tag commands reads an entry the same way.
  expect(invokeMock).toHaveBeenCalledWith("add_video_tags", {
    resultId: "video-1",
    tags: ["vacation house"],
  });
  for (const tag of ["house", "old", "vacation"]) {
    expect(
      await screen.findByRole("button", { name: `Remove tag ${tag}` }),
    ).toBeVisible();
  }
});

test("draws the tag's remove control as its own icon beside the name", async () => {
  invokeMock.mockResolvedValueOnce({
    query: "tagged",
    page: 1,
    pageSize: 24,
    totalResults: 1,
    totalPages: 1,
    results: [
      {
        id: "video-1",
        fileName: "tagged.mp4",
        extension: "mp4",
        tags: ["work"],
      },
    ],
  });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "tagged{Enter}");

  const pill = await screen.findByRole("button", { name: "Remove tag work" });

  // A text "×" sat on the name's baseline rather than its centre, and the two
  // ran together. The name is the pill's only text now; the cross is a drawn
  // glyph the stylesheet can centre and space on its own.
  expect(pill).toHaveTextContent(/^work$/);
  expect(pill.querySelector(".tag-remove-glyph")).toBeInTheDocument();
});

test("gives the open tag field the keyboard and pauses for it", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "tagged",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [
        {
          id: "video-1",
          fileName: "tagged.mp4",
          extension: "mp4",
          tags: ["work"],
        },
      ],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/tagged.mp4" })
    .mockResolvedValue({ fileName: "tagged [review work].mp4", tags: [] });
  const requestFullscreen = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: requestFullscreen,
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "tagged{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play tagged.mp4" }),
  );
  const playing = await screen.findByLabelText("Playing tagged.mp4");
  fireEvent.play(playing);
  expect(screen.getByRole("button", { name: "Pause" })).toBeVisible();
  const paused = vi
    .spyOn(HTMLMediaElement.prototype, "pause")
    .mockImplementation(() => {});

  fireEvent.keyDown(window, { key: "t" });
  const tagField = screen.getByRole("textbox", {
    name: "Add tag to tagged.mp4",
  });
  expect(tagField).toHaveFocus();
  // The picture waits while the viewer types.
  expect(paused).toHaveBeenCalled();
  expect(screen.getByRole("button", { name: "Play" })).toBeVisible();

  // Shortcut letters reach the field instead of running their commands.
  await user.keyboard("far");
  expect(tagField).toHaveValue("far");
  expect(requestFullscreen).not.toHaveBeenCalled();

  // Nor are they commands when the field is open without holding focus, which
  // is the state fullscreen used to leave it in.
  tagField.blur();
  fireEvent.keyDown(window, { key: "f" });
  expect(requestFullscreen).not.toHaveBeenCalled();

  // Escape closes the field and leaves the player where it was, rather than
  // falling through to the grid.
  fireEvent.keyDown(window, { key: "Escape" });
  expect(
    screen.queryByRole("textbox", { name: "Add tag to tagged.mp4" }),
  ).toBeNull();
  expect(await screen.findByLabelText("Video controls")).toBeVisible();
});

test("leaves the results tag control without a keyboard shortcut", async () => {
  invokeMock.mockResolvedValueOnce({
    query: "tagged",
    page: 1,
    pageSize: 24,
    totalResults: 1,
    totalPages: 1,
    results: [
      {
        id: "video-1",
        fileName: "tagged.mp4",
        extension: "mp4",
        tags: ["work"],
      },
    ],
  });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "tagged{Enter}");

  // Nothing on the grid claims T: a keystroke there could not say which video
  // it meant. The control is still there for the pointer.
  const addTag = await screen.findByRole("button", {
    name: "Add tag to tagged.mp4",
  });
  expect(addTag).not.toHaveAttribute("aria-keyshortcuts");
  expect(addTag.querySelector(".key-hint")).toBeNull();
  expect(addTag).toHaveAttribute("title", "Add tag to tagged.mp4");
  await user.click(addTag);
  expect(
    screen.getByRole("textbox", { name: "Add tag to tagged.mp4" }),
  ).toHaveFocus();
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );

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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );

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

  for (const name of [
    "Previous video",
    "Skip back 10 seconds",
    "Play",
    "Skip forward 10 seconds",
    "Next video",
  ]) {
    expect(transport).toContainElement(screen.getByRole("button", { name }));
  }
  expect(transport).toContainElement(screen.getByText("0:00 / 0:00"));

  for (const name of [
    "Rotate left",
    "Rotate right",
    "Loop: playlist",
    "Enter fullscreen",
  ]) {
    expect(utilities).toContainElement(screen.getByRole("button", { name }));
  }
  expect(utilities).toContainElement(
    screen.getByRole("combobox", { name: "Playback speed" }),
  );

  const play = screen.getByRole("button", { name: "Play" });
  expect(play).toHaveClass("play-button");
  expect(play.querySelector(".play-glyph")).toBeInTheDocument();
});

test("edits the playing video's tags from the playback controls with T", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "tagged",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [
        {
          id: "video-1",
          fileName: "tagged.mp4",
          extension: "mp4",
          tags: ["work"],
        },
      ],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/tagged.mp4" })
    .mockResolvedValueOnce({
      fileName: "tagged [review work].mp4",
      tags: ["review", "work"],
    })
    .mockResolvedValueOnce({
      fileName: "tagged [review].mp4",
      tags: ["review"],
    });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "tagged{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play tagged.mp4" }),
  );
  const controls = await screen.findByLabelText("Video controls");
  expect(controls).toContainElement(
    screen.getByRole("button", { name: "Remove tag work" }),
  );

  fireEvent.keyDown(window, { key: "t" });
  const tagInput = screen.getByRole("textbox", {
    name: "Add tag to tagged.mp4",
  });
  expect(tagInput).toBeVisible();
  await user.type(tagInput, "review{Enter}");
  expect(
    await screen.findByRole("button", { name: "Remove tag review" }),
  ).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Remove tag work" }));

  expect(invokeMock).toHaveBeenCalledWith("add_video_tags", {
    resultId: "video-1",
    tags: ["review"],
  });
  expect(invokeMock).toHaveBeenCalledWith("remove_video_tags", {
    resultId: "video-1",
    tags: ["work"],
  });
});

test("uses one control for playing and pausing", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");

  // Playing and pausing are the same action read through the current state, so
  // only ever one of the two is on screen.
  const transport = document.querySelector(".player-transport");
  expect(transport?.querySelectorAll(".play-button")).toHaveLength(1);
  expect(
    screen.queryByRole("button", { name: "Pause" }),
  ).not.toBeInTheDocument();

  fireEvent.play(video);
  const pause = screen.getByRole("button", { name: "Pause" });
  expect(pause).toHaveClass("play-button");
  expect(pause.querySelector(".pause-glyph")).toBeInTheDocument();
  expect(pause).toHaveAttribute("aria-keyshortcuts", "Space");
  expect(
    screen.queryByRole("button", { name: "Play" }),
  ).not.toBeInTheDocument();

  // The one button drives both directions, from the pointer and from Space.
  const paused = vi
    .spyOn(HTMLMediaElement.prototype, "pause")
    .mockImplementation(() => {});
  await user.click(pause);
  expect(paused).toHaveBeenCalled();
  fireEvent.pause(video);
  expect(screen.getByRole("button", { name: "Play" })).toBeVisible();

  const played = vi
    .spyOn(HTMLMediaElement.prototype, "play")
    .mockResolvedValue(undefined);
  fireEvent.keyDown(window, { key: " " });
  expect(played).toHaveBeenCalled();
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Pause" })).toBeVisible(),
  );
});

test("starts the whole result page as a playlist positioned at the chosen video", async () => {
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
    .mockResolvedValue({ filePath: "/Videos/playlist-2.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play playlist-2.mp4" }),
  );

  // Choosing one result drops the viewer into the page's playlist at that spot
  // rather than playing it on its own.
  expect(await screen.findByLabelText("Playing playlist-2.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 2 of 3")).toBeVisible();
  expect(screen.getByRole("button", { name: "Playlist 3" })).toBeVisible();
  expect(screen.getAllByRole("listitem")).toHaveLength(3);
  expect(
    screen.getByRole("button", { name: "playlist-2.mp4" }),
  ).toHaveAttribute("aria-current", "true");

  // And the rest of the page is reachable from there in both directions.
  expect(screen.getByRole("button", { name: "Previous video" })).toBeEnabled();
  expect(screen.getByRole("button", { name: "Next video" })).toBeEnabled();
});

test("deletes the current video, advances, and restores it with the undo shortcut", async () => {
  const results = [1, 2, 3].map((number) => ({
    id: `video-${number}`,
    fileName: `delete-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "delete",
      page: 1,
      pageSize: 24,
      totalResults: 3,
      totalPages: 1,
      results,
    })
    .mockResolvedValue({ filePath: "/Videos/delete.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "delete{Enter}");
  await user.click(screen.getByRole("button", { name: "Play delete-2.mp4" }));
  expect(await screen.findByLabelText("Playing delete-2.mp4")).toBeVisible();
  await user.click(screen.getByRole("button", { name: "Delete video" }));
  expect(await screen.findByLabelText("Playing delete-3.mp4")).toBeVisible();
  expect(invokeMock).toHaveBeenCalledWith("delete_video", {
    resultId: "video-2",
  });
  fireEvent.keyDown(window, { key: "Delete", shiftKey: true, ctrlKey: true });
  expect(await screen.findByLabelText("Playing delete-2.mp4")).toBeVisible();
  expect(invokeMock).toHaveBeenCalledWith("undo_delete");
});

// The overlay's own scrubber, volume slider and dropdowns used to swallow every
// shortcut: once one of them held focus the window handler bailed out on the
// "someone is typing" check, so Shift+Delete deleted nothing and Space stopped
// playing and pausing, with no sign of why.
test("keeps its shortcuts working while a player control holds focus", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `focus-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "focus",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
    .mockResolvedValue({ filePath: "/Videos/focus.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "focus{Enter}");
  await user.click(screen.getByRole("button", { name: "Play focus-1.mp4" }));
  expect(await screen.findByLabelText("Playing focus-1.mp4")).toBeVisible();

  const scrubber = screen.getByRole("slider", { name: "Video timeline" });
  const speed = screen.getByRole("combobox", { name: "Playback speed" });
  for (const control of [scrubber, speed]) {
    control.focus();
    fireEvent.keyDown(control, { key: "]" });
  }

  // Two rotations, one from each control, and neither was swallowed.
  expect(screen.getByLabelText("Playing focus-1.mp4")).toHaveStyle({
    transform: "rotate(180deg)",
  });

  speed.focus();
  fireEvent.keyDown(speed, { key: "Delete", shiftKey: true });
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("delete_video", {
      resultId: "video-1",
    }),
  );
});

test("hands focus back to the player after a control is used", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "focus",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [
        { id: "video-1", fileName: "focus.mp4", extension: "mp4", tags: [] },
      ],
    })
    .mockResolvedValue({ filePath: "/Videos/focus.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "focus{Enter}");
  await user.click(screen.getByRole("button", { name: "Play focus.mp4" }));
  expect(await screen.findByLabelText("Playing focus.mp4")).toBeVisible();

  // A clicked control kept focus, and with it a ring that stayed lit long after
  // the click, on a button that was no longer doing anything.
  await user.click(screen.getByRole("button", { name: "Rotate right" }));
  expect(document.activeElement).toHaveClass("player-shell");

  await user.selectOptions(
    screen.getByRole("combobox", { name: "Playback speed" }),
    "2",
  );
  expect(document.activeElement).toHaveClass("player-shell");

  // The tag field is the one control that has to keep what it is given.
  await user.click(
    screen.getByRole("button", { name: "Add tag to focus.mp4" }),
  );
  const field = await screen.findByRole("textbox", {
    name: "Add tag to focus.mp4",
  });
  expect(field).toHaveFocus();
});

test("spells out every key in a shortcut with more than one", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValue({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(screen.getByRole("button", { name: "Play clip.mp4" }));

  // "ShiftDelete" read as one unpressable key.
  const remove = await screen.findByRole("button", { name: "Delete video" });
  expect(remove.querySelector(".key-hint")).toHaveTextContent("Shift+Delete");
});

test("shows a sidecar subtitle track and turns it off again", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "clip",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
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
    if (command === "subtitle_cues")
      return Promise.resolve("WEBVTT\n\n00:01.000 --> 00:02.000\nHi");
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );

  const toggle = await screen.findByRole("button", { name: "Subtitles" });
  expect(toggle).toHaveAttribute("aria-pressed", "false");
  expect(toggle).toHaveAttribute("aria-keyshortcuts", "S");
  expect(toggle).toBeEnabled();

  await user.click(toggle);
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute(
      "aria-pressed",
      "true",
    ),
  );
  expect(invokeMock).toHaveBeenCalledWith("subtitle_cues", {
    resultId: "video-1",
    track: 0,
  });

  const track = document.querySelector("video")?.textTracks[0];
  expect(track?.label).toBe("Subtitles");
  expect(track?.mode).toBe("showing");
  expect((track?.cues?.[0] as VTTCue | undefined)?.text).toBe("Hi");

  await user.click(screen.getByRole("button", { name: "Subtitles" }));
  expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
  expect(track?.mode).toBe("disabled");
});

test("switches between the subtitle tracks found beside the video", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "clip",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  await user.click(await screen.findByRole("button", { name: "Subtitles" }));

  const chooser = await screen.findByRole("combobox", {
    name: "Subtitle track",
  });
  // The .ass sidecar is listed by Rust but the web engine cannot render it.
  expect(
    [...chooser.querySelectorAll("option")].map((option) => option.textContent),
  ).toEqual(["Off", "Subtitles", "EN"]);

  await user.selectOptions(chooser, "1");
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("subtitle_cues", {
      resultId: "video-1",
      track: 1,
    }),
  );
  const tracks = document.querySelector("video")?.textTracks;
  expect(tracks?.[tracks.length - 1]?.language).toBe("en");
});

test("toggles subtitles with the keyboard and disables the control without tracks", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4", subtitles: [] });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );

  // Visible but inert, so the feature stays discoverable when a video has none.
  const toggle = await screen.findByRole("button", { name: "Subtitles" });
  expect(toggle).toBeVisible();
  expect(toggle).toBeDisabled();

  fireEvent.keyDown(window, { key: "s" });
  expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
  expect(
    screen.queryByRole("combobox", { name: "Subtitle track" }),
  ).not.toBeInTheDocument();
});

test("selects and clears the mpv subtitle track for native playback", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [{ id: "native-1", fileName: "native.mkv", extension: "mkv" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/native.mkv",
        playbackBackend: "native",
        subtitles: [],
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({
        duration: 120,
        currentTime: 1,
        paused: false,
        ended: false,
      });
    }
    if (command === "native_subtitle_tracks") {
      return Promise.resolve([{ id: 1, label: "English", external: false }]);
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play native.mkv" }),
  );

  // mpv reports embedded tracks only once the file has loaded.
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Subtitles" })).toBeEnabled(),
  );

  fireEvent.keyDown(window, { key: "s" });
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_subtitle", { id: 1 }),
  );
  expect(screen.getByRole("button", { name: "Subtitles" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );

  fireEvent.keyDown(window, { key: "s" });
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_subtitle", {
      id: null,
    }),
  );
});

test("shows its keyboard shortcut on every control that has one", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results: [1, 2].map((n) => ({
        id: `video-${n}`,
        fileName: `clip-${n}.mp4`,
        extension: "mp4",
      })),
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
    const hint =
      control.querySelector(".key-hint") ??
      control.parentElement?.querySelector(".key-hint");
    expect(
      hint,
      `${control.getAttribute("aria-label")} has no visible shortcut`,
    ).toBeTruthy();
  }

  // The example from the issue: 'rotate right' is bound to ']' and says so.
  const rotateRight = screen.getByRole("button", { name: "Rotate right" });
  expect(rotateRight).toHaveAttribute("aria-keyshortcuts", "]");
  expect(rotateRight.querySelector(".key-hint")).toHaveTextContent("]");

  // Hints read as key names rather than raw DOM ones.
  expect(
    screen
      .getByRole("button", { name: "Next video" })
      .querySelector(".key-hint"),
  ).toHaveTextContent("PgDn");
  expect(
    screen
      .getByRole("button", { name: "Previous video" })
      .querySelector(".key-hint"),
  ).toHaveTextContent("PgUp");
  expect(
    screen
      .getByRole("button", { name: "Back to results" })
      .querySelector(".key-hint"),
  ).toHaveTextContent("Esc");
});

test("steps playback speed with the keyboard", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
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
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockRejectedValueOnce(
      new Error(
        "This video format or codec is not supported on this computer.",
      ),
    );
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );

  expect(
    await screen.findByRole("heading", {
      name: "This video format isn't supported on your computer",
    }),
  ).toBeVisible();
  expect(screen.getByRole("button", { name: "Back to results" })).toBeVisible();
});

test("enters fullscreen mode for the player", async () => {
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
  const requestFullscreen = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: requestFullscreen,
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  await user.click(
    await screen.findByRole("button", { name: "Enter fullscreen" }),
  );

  expect(requestFullscreen).toHaveBeenCalledOnce();
});

test("cycles F through video, information, controls, and windowed modes", async () => {
  const userAgent = vi
    .spyOn(window.navigator, "userAgent", "get")
    .mockReturnValue("AppleWebKit Mac OS X");
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

  try {
    await user.type(screen.getByRole("searchbox"), "clip{Enter}");
    await user.click(
      await screen.findByRole("button", { name: "Play clip.mp4" }),
    );
    const fullscreenControl = screen.getByRole("button", {
      name: "Enter fullscreen",
    });
    expect(fullscreenControl).toHaveAttribute("aria-keyshortcuts", "F");

    fireEvent.keyDown(window, { key: "f" });
    await waitFor(() =>
      expect(windowApiMock.setFullscreen).toHaveBeenCalledWith(true),
    );
    await waitFor(() =>
      expect(document.querySelector(".player-shell")).toHaveClass("fullscreen"),
    );
    expect(document.querySelector(".player-shell")).toHaveClass("video-only");
    expect(
      screen.queryByRole("region", { name: "Fullscreen video information" }),
    ).not.toBeInTheDocument();
    expect(screen.getByLabelText("Video controls")).not.toBeVisible();

    fireEvent.keyDown(window, { key: "f" });
    expect(document.querySelector(".player-shell")).not.toHaveClass(
      "video-only",
    );
    expect(
      screen.getByRole("region", { name: "Fullscreen video information" }),
    ).toBeVisible();
    expect(screen.getByLabelText("Video controls")).toHaveClass("idle");

    fireEvent.keyDown(window, { key: "f" });
    await waitFor(() =>
      expect(screen.getByLabelText("Video controls")).not.toHaveClass("idle"),
    );

    fireEvent.keyDown(window, { key: "f" });
    await waitFor(() =>
      expect(windowApiMock.setFullscreen).toHaveBeenLastCalledWith(false),
    );
    expect(
      screen.getByRole("button", { name: "Enter fullscreen" }),
    ).toHaveAttribute("aria-keyshortcuts", "F");
  } finally {
    userAgent.mockRestore();
  }
});

test("shows fullscreen path and time overlays and toggles them with I", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen();
    await enterFullscreen(user);

    const info = screen.getByRole("region", {
      name: "Fullscreen video information",
    });
    expect(info).toBeVisible();
    expect(within(info).getByText("/Videos/clip.mp4")).toBeVisible();
    expect(info).toHaveTextContent("0:00 / 0:00");
    // The overlay is the only thing left telling a viewer what they are
    // watching and how far in they are, so it outlives the controls it was
    // revealed alongside.
    expect(screen.getByLabelText("Video controls")).toHaveClass("idle");
    expect(
      screen.getByRole("button", { name: "Hide fullscreen information" }),
    ).toHaveAttribute("aria-keyshortcuts", "I");

    fireEvent.keyDown(window, { key: "i" });
    expect(
      screen.queryByRole("region", { name: "Fullscreen video information" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Show fullscreen information" }),
    ).toBeVisible();
  } finally {
    vi.useRealTimers();
  }
});

test("names the file and its folder in the windowed heading", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    await playForFullscreen();
    const heading = await screen.findByRole("heading", { name: "clip.mp4" });

    // The windowed player already names the file; the folder it came from is
    // the part that used to be missing, and it is what the fullscreen overlay
    // exists to supply.
    expect(heading).toBeVisible();
    expect(screen.getByText("/Videos/clip.mp4")).toBeVisible();
  } finally {
    vi.useRealTimers();
  }
});

test("offers the info control only where it can change anything", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  try {
    const user = await playForFullscreen();
    await screen.findByLabelText("Video controls");

    // Windowed, the heading already shows the name and path and the controls
    // already show the time, so there is nothing for the toggle to reveal.
    expect(
      screen.queryByRole("button", { name: /fullscreen information/ }),
    ).not.toBeInTheDocument();
    fireEvent.keyDown(window, { key: "i" });
    expect(
      screen.queryByRole("region", { name: "Fullscreen video information" }),
    ).not.toBeInTheDocument();

    await enterFullscreen(user);
    expect(
      screen.getByRole("button", { name: "Hide fullscreen information" }),
    ).toBeVisible();
  } finally {
    reportFullscreen(false);
    vi.useRealTimers();
  }
});

test("uses Tauri window fullscreen when the web fullscreen request fails", async () => {
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
  const requestFullscreen = vi
    .fn()
    .mockRejectedValue(new Error("Web fullscreen failed"));
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: requestFullscreen,
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  await user.click(
    await screen.findByRole("button", { name: "Enter fullscreen" }),
  );

  await waitFor(() =>
    expect(windowApiMock.setFullscreen).toHaveBeenCalledWith(true),
  );
  expect(
    screen.queryByRole("heading", { name: "This video could not be played" }),
  ).not.toBeInTheDocument();
  expect(screen.getByLabelText("Playing clip.mp4")).toBeVisible();
  expect(screen.getByLabelText("Video controls")).not.toBeVisible();
});

// WebKitGTK's own fullscreen mode takes keys before the page is offered them:
// `f` leaves fullscreen there, so a tag with an f in it could not be typed
// while the player was fullscreen, and the letter never reached the field.
// Tauri's window fullscreen looks the same to the viewer and leaves every
// keystroke to the app.
test("uses Tauri window fullscreen first on Linux", async () => {
  const userAgent = vi
    .spyOn(window.navigator, "userAgent", "get")
    .mockReturnValue("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15");
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
  const requestFullscreen = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: requestFullscreen,
  });
  const user = userEvent.setup();
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "clip{Enter}");
    await user.click(
      await screen.findByRole("button", { name: "Play clip.mp4" }),
    );
    await user.click(
      await screen.findByRole("button", { name: "Enter fullscreen" }),
    );

    await waitFor(() =>
      expect(windowApiMock.setFullscreen).toHaveBeenCalledWith(true),
    );
    expect(requestFullscreen).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Video controls")).not.toBeVisible();
  } finally {
    userAgent.mockRestore();
  }
});

test("uses Tauri window fullscreen first on macOS", async () => {
  const userAgent = vi
    .spyOn(window.navigator, "userAgent", "get")
    .mockReturnValue("AppleWebKit Mac OS X");
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
  const requestFullscreen = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: requestFullscreen,
  });
  const user = userEvent.setup();
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "clip{Enter}");
    await user.click(
      await screen.findByRole("button", { name: "Play clip.mp4" }),
    );
    await user.click(
      await screen.findByRole("button", { name: "Enter fullscreen" }),
    );

    await waitFor(() =>
      expect(windowApiMock.setFullscreen).toHaveBeenCalledWith(true),
    );
    expect(requestFullscreen).not.toHaveBeenCalled();
    expect(
      screen.queryByRole("heading", { name: "This video could not be played" }),
    ).not.toBeInTheDocument();
    expect(screen.getByLabelText("Playing clip.mp4")).toBeVisible();
  } finally {
    userAgent.mockRestore();
  }
});

test("keeps player keyboard shortcuts live after leaving fullscreen from the keyboard", async () => {
  const userAgent = vi
    .spyOn(window.navigator, "userAgent", "get")
    .mockReturnValue("AppleWebKit Mac OS X");
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

  try {
    await user.type(screen.getByRole("searchbox"), "clip{Enter}");
    await user.click(
      await screen.findByRole("button", { name: "Play clip.mp4" }),
    );
    const video = await screen.findByLabelText("Playing clip.mp4");

    fireEvent.keyDown(window, { key: "f" });
    await waitFor(() =>
      expect(document.querySelector(".player-shell")).toHaveClass("fullscreen"),
    );
    fireEvent.keyDown(window, { key: "f" });
    await screen.findByRole("region", {
      name: "Fullscreen video information",
    });
    fireEvent.keyDown(window, { key: "f" });
    await waitFor(() =>
      expect(screen.getByLabelText("Video controls")).not.toHaveClass("idle"),
    );
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
  await user.click(
    await screen.findByRole("button", { name: "Enter fullscreen" }),
  );
  reportFullscreen(true);
  // Most legacy fullscreen tests exercise the information/scrubber mode that
  // used to be the only entry state. The first F now advances there from the
  // new video-only state.
  fireEvent.keyDown(window, { key: "f" });
  await screen.findByRole("region", {
    name: "Fullscreen video information",
  });
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
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: results.length,
      totalPages: 1,
      results,
    })
    .mockResolvedValue({ filePath: `/Videos/${names[0]}` });
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: vi.fn().mockResolvedValue(undefined),
  });
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(
    await screen.findByRole("button", {
      name: names.length > 1 ? "Play all" : `Play ${names[0]}`,
    }),
  );
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
    expect(
      screen.getByRole("complementary", { name: "Playlist" }),
    ).toBeVisible();

    await enterFullscreen(user);

    // Straight away, not after the idle delay: fullscreen is for watching.
    expect(controls).toHaveClass("idle");
    expect(
      screen.queryByRole("complementary", { name: "Playlist" }),
    ).not.toBeInTheDocument();
    // Everything except the scrubber goes; it is the only feedback left about
    // how far into the video the viewer is.
    expect(screen.getByLabelText("Video timeline")).toBeInTheDocument();
    expect(controls).toContainElement(screen.getByLabelText("Video timeline"));

    // Coming back out restores the windowed player as it was left.
    reportFullscreen(false);
    expect(controls).not.toHaveClass("idle");
    expect(
      screen.getByRole("complementary", { name: "Playlist" }),
    ).toBeVisible();
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

const playlistDrawer = () =>
  screen.queryByRole("complementary", { name: "Playlist" });

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
    expect(screen.getByRole("button", { name: "Playlist" })).toHaveAttribute(
      "aria-keyshortcuts",
      "P",
    );

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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");

  // Looping a playlist of one means playing that one again; wrapping to the
  // index it is already on is a state change React would throw away.
  const play = vi
    .spyOn(HTMLMediaElement.prototype, "play")
    .mockResolvedValue(undefined);
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
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
    .mockResolvedValue({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  const video = await screen.findByLabelText("Playing playlist-1.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  fireEvent.loadedMetadata(video);

  await user.click(loopControl());
  const play = vi
    .spyOn(HTMLMediaElement.prototype, "play")
    .mockResolvedValue(undefined);
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
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
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

  const play = vi
    .spyOn(HTMLMediaElement.prototype, "play")
    .mockResolvedValue(undefined);
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
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
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
  Object.defineProperty(video, "currentTime", {
    configurable: true,
    writable: true,
    value: 112,
  });
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");

  await user.click(screen.getByRole("button", { name: "Rotate right" }));
  expect(video).toHaveStyle({ transform: "rotate(90deg)" });

  await user.click(screen.getByRole("button", { name: "Rotate left" }));
  expect(video).toHaveStyle({ transform: "rotate(0deg)" });
});

test("changes playback speed for web video", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");
  await user.selectOptions(
    screen.getByRole("combobox", { name: "Playback speed" }),
    "1.5",
  );
  expect(video).toHaveProperty("playbackRate", 1.5);
});

test("offers tooltips for every actionable result and player control", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 2,
      results: [
        { id: "video-1", fileName: "clip.mp4", extension: "mp4" },
        { id: "video-2", fileName: "other.mp4", extension: "mp4" },
      ],
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  expect(screen.getByRole("button", { name: "Clear search" })).toHaveAttribute(
    "title",
    "Clear search",
  );
  expect(screen.getByRole("button", { name: "Play all" })).toHaveAttribute(
    "title",
    "Play all videos",
  );
  expect(screen.getByRole("button", { name: "Play clip.mp4" })).toHaveAttribute(
    "title",
    "Play clip.mp4",
  );
  expect(screen.getByText("2 of 2 loaded")).toBeVisible();

  await user.click(screen.getByRole("button", { name: "Play clip.mp4" }));
  const controls = await screen.findByLabelText("Video controls");
  for (const control of controls.querySelectorAll<
    HTMLButtonElement | HTMLSelectElement
  >("button, select")) {
    expect(
      control,
      `${control.getAttribute("aria-label")} has no tooltip`,
    ).toHaveAttribute("title");
    expect(control.title).not.toBe("");
  }
  expect(
    screen.getByRole("button", { name: "Back to results" }),
  ).toHaveAttribute("title", "Back to results");
  expect(screen.getByRole("button", { name: "clip.mp4" })).toHaveAttribute(
    "title",
    "Play clip.mp4",
  );
});

test("supports playback speeds from 0.1x through 4x", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  Object.defineProperty(video, "currentTime", {
    configurable: true,
    writable: true,
    value: 20,
  });
  fireEvent.timeUpdate(video);
  await user.click(
    screen.getByRole("button", { name: "Skip forward 10 seconds" }),
  );
  expect((video as HTMLVideoElement).currentTime).toBe(30);
});

test("uses left and right arrows for ten-second seeking", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  Object.defineProperty(video, "currentTime", {
    configurable: true,
    writable: true,
    value: 20,
  });
  fireEvent.timeUpdate(video);

  fireEvent.keyDown(window, { key: "ArrowLeft" });
  expect(video).toHaveProperty("currentTime", 10);
  fireEvent.keyDown(window, { key: "ArrowRight" });
  expect(video).toHaveProperty("currentTime", 20);
  expect(
    screen.getByRole("button", { name: "Skip back 10 seconds" }),
  ).toHaveAttribute("aria-keyshortcuts", "ArrowLeft");
  expect(
    screen.getByRole("button", { name: "Skip forward 10 seconds" }),
  ).toHaveAttribute("aria-keyshortcuts", "ArrowRight");
});

test("changes volume with the discoverable keyboard shortcuts", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  await screen.findByLabelText("Playing clip.mp4");

  const volume = screen.getByRole("slider", { name: "Volume" });
  expect(volume).toHaveValue("100");
  fireEvent.keyDown(window, { key: "ArrowDown" });
  expect(volume).toHaveValue("95");
  fireEvent.keyDown(window, { key: "ArrowUp" });
  expect(volume).toHaveValue("100");

  const quieter = screen.getByRole("button", { name: "Decrease volume" });
  const louder = screen.getByRole("button", { name: "Increase volume" });
  expect(quieter).toHaveAttribute("aria-keyshortcuts", "ArrowDown");
  expect(louder).toHaveAttribute("aria-keyshortcuts", "ArrowUp");
  expect(quieter.querySelector(".key-hint")).toHaveTextContent("↓");
  expect(louder.querySelector(".key-hint")).toHaveTextContent("↑");

  // Stepping the volume digit by digit was awkward to reach: only the three
  // presets keep a digit, and the rest of the row does nothing.
  fireEvent.keyDown(window, { key: "9" });
  fireEvent.keyDown(window, { key: "3" });
  expect(volume).toHaveValue("100");

  // "Vol −" and "Vol +" as words sat oddly among a row of icons; a speaker
  // carrying more or fewer waves says the same thing at a glance.
  expect(quieter).not.toHaveTextContent("Vol");
  expect(louder).not.toHaveTextContent("Vol");
  expect(quieter.querySelector(".control-icon")).toBeInTheDocument();
  expect(louder.querySelector(".control-icon")).toBeInTheDocument();
});

test("jumps to preset volumes with the discoverable digit shortcuts", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");

  const volume = screen.getByRole("slider", { name: "Volume" });
  expect(volume).toHaveValue("100");

  fireEvent.keyDown(window, { key: "0" });
  expect(volume).toHaveValue("0");
  expect(video).toHaveProperty("volume", 0);

  fireEvent.keyDown(window, { key: "5" });
  expect(volume).toHaveValue("50");
  expect(video).toHaveProperty("volume", 0.5);

  fireEvent.keyDown(window, { key: "1" });
  expect(volume).toHaveValue("100");
  expect(video).toHaveProperty("volume", 1);

  // The arrow keys still step from wherever a preset left the volume.
  fireEvent.keyDown(window, { key: "0" });
  fireEvent.keyDown(window, { key: "ArrowUp" });
  expect(volume).toHaveValue("5");

  const mute = screen.getByRole("button", { name: "Mute" });
  const half = screen.getByRole("button", { name: "Half volume" });
  const full = screen.getByRole("button", { name: "Full volume" });
  expect(mute).toHaveAttribute("aria-keyshortcuts", "0");
  expect(half).toHaveAttribute("aria-keyshortcuts", "5");
  expect(full).toHaveAttribute("aria-keyshortcuts", "1");
  expect(mute.querySelector(".key-hint")).toHaveTextContent("0");
  expect(half.querySelector(".key-hint")).toHaveTextContent("5");
  expect(full.querySelector(".key-hint")).toHaveTextContent("1");

  // The mute control reads as pressed while the sound is off, and clicking
  // any preset moves the volume the same way its key does.
  await user.click(mute);
  expect(volume).toHaveValue("0");
  expect(mute).toHaveAttribute("aria-pressed", "true");
  await user.click(half);
  expect(volume).toHaveValue("50");
  expect(mute).toHaveAttribute("aria-pressed", "false");
  await user.click(full);
  expect(volume).toHaveValue("100");
});

test("sends preset volumes to native playback", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [{ id: "native-1", fileName: "native.mp4", extension: "mp4" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/native.mp4",
        playbackBackend: "native",
      });
    }
    if (command === "native_playback_state") {
      return Promise.resolve({ currentTime: 0, duration: 30, ended: false });
    }
    return Promise.resolve(null);
  });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play native.mp4" }),
  );
  await screen.findByLabelText("Playing native.mp4");

  fireEvent.keyDown(window, { key: "0" });
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_volume", {
      volume: 0,
    }),
  );
  fireEvent.keyDown(window, { key: "5" });
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_volume", {
      volume: 50,
    }),
  );
  fireEvent.keyDown(window, { key: "1" });
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_volume", {
      volume: 100,
    }),
  );
});

test("sends the selected rotation to native playback", async () => {
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [{ id: "native-1", fileName: "native.mp4", extension: "mp4" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/native.mp4",
        playbackBackend: "native",
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({
        duration: 120,
        currentTime: 1,
        paused: false,
        ended: false,
      });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play native.mp4" }),
  );
  await user.click(await screen.findByRole("button", { name: "Rotate left" }));

  expect(invokeMock).toHaveBeenCalledWith("set_native_video_rotation", {
    degrees: 270,
  });
});

test("keyboard shortcuts control player actions without hijacking search input", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `clip-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
    .mockResolvedValueOnce({ filePath: "/Videos/clip-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/clip-2.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/clip-1.mp4" });
  const requestFullscreen = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(HTMLElement.prototype, "requestFullscreen", {
    configurable: true,
    value: requestFullscreen,
  });
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
  expect(screen.getByLabelText("Playing clip-1.mp4")).toHaveStyle({
    transform: "rotate(0deg)",
  });
});

test("keeps keyboard shortcuts active after a transport button is clicked", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  Object.defineProperty(video, "currentTime", {
    configurable: true,
    writable: true,
    value: 20,
  });
  fireEvent.timeUpdate(video);

  const skipBack = screen.getByRole("button", { name: "Skip back 10 seconds" });
  await user.click(skipBack);
  // The button used to keep the focus it was given, and the ring that came with
  // it, on a control that had already done its work.
  expect(skipBack).not.toHaveFocus();
  fireEvent.keyDown(document.activeElement ?? window, { key: " " });

  expect(video).toHaveProperty("paused", true);
});

test("loops a playlist back to its first video", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
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
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      private readonly callback: IntersectionObserverCallback;
      constructor(callback: IntersectionObserverCallback) {
        this.callback = callback;
      }
      observe(target: Element) {
        if (target.classList.contains("load-more-marker")) {
          this.callback(
            [{ isIntersecting: true } as IntersectionObserverEntry],
            this as unknown as IntersectionObserver,
          );
        }
      }
      disconnect() {}
      unobserve() {}
      takeRecords() {
        return [];
      }
      root = null;
      rootMargin = "0px";
      thresholds = [];
    },
  );
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  expect(
    await screen.findByRole("button", { name: "Play clip-24.mp4" }),
  ).toBeVisible();
  expect(screen.getByText("2 of 25 loaded")).toBeVisible();

  await user.clear(screen.getByRole("searchbox"));
  await user.type(screen.getByRole("searchbox"), "broken{Enter}");
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Recoll search could not start.",
  );
  await waitFor(() =>
    expect(screen.getByRole("searchbox")).toHaveValue("broken"),
  );
});

test("keeps loading more results after coming back from a video", async () => {
  const pageOf = (start: number) =>
    [start, start + 1, start + 2].map((number) => ({
      id: `video-${number}`,
      fileName: `clip-${number}.mp4`,
      extension: "mp4",
    }));
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "search_videos") {
      const requested = (args as { request: { page: number } }).request.page;
      return Promise.resolve({
        query: "clip",
        page: requested,
        pageSize: 24,
        totalResults: 6,
        totalPages: 2,
        results: pageOf(requested === 1 ? 1 : 4),
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({ filePath: "/Videos/clip-1.mp4" });
    }
    return Promise.resolve();
  });
  // Faithful enough to catch the bug this covers: an observer only reports a
  // target that is still in the document, and reports nothing once it has been
  // disconnected. An observer left watching the marker from a previous mount
  // therefore never fires, exactly as in a browser.
  const observers: {
    disconnected: boolean;
    targets: Set<Element>;
    report: () => void;
  }[] = [];
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      readonly targets = new Set<Element>();
      disconnected = false;
      constructor(private readonly callback: IntersectionObserverCallback) {
        observers.push(this);
      }
      report() {
        if (this.disconnected) return;
        const visible = [...this.targets].filter(
          (target) => target.isConnected,
        );
        if (!visible.length) return;
        this.callback(
          visible.map(
            (target) =>
              ({
                target,
                isIntersecting: true,
              }) as unknown as IntersectionObserverEntry,
          ),
          this as unknown as IntersectionObserver,
        );
      }
      observe(target: Element) {
        if (target.classList.contains("load-more-marker"))
          this.targets.add(target);
      }
      disconnect() {
        this.disconnected = true;
        this.targets.clear();
      }
      unobserve(target: Element) {
        this.targets.delete(target);
      }
      takeRecords() {
        return [];
      }
      root = null;
      rootMargin = "0px";
      thresholds = [];
    },
  );
  const scrollToTheEnd = async () => {
    await act(async () => {
      for (const observer of observers) observer.report();
    });
  };
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  expect(await screen.findByText("3 of 6 loaded")).toBeVisible();

  await user.click(
    await screen.findByRole("button", { name: "Play clip-1.mp4" }),
  );
  await user.click(
    await screen.findByRole("button", { name: "Back to results" }),
  );
  expect(await screen.findByText("3 of 6 loaded")).toBeVisible();

  await scrollToTheEnd();

  expect(await screen.findByText("6 of 6 loaded")).toBeVisible();
  expect(
    await screen.findByRole("button", { name: "Play clip-6.mp4" }),
  ).toBeVisible();
});

test("returns to where the results were scrolled to after a video", async () => {
  const results = [1, 2, 3].map((number) => ({
    id: `video-${number}`,
    fileName: `clip-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos")
      return Promise.resolve({
        query: "clip",
        page: 1,
        pageSize: 24,
        totalResults: 3,
        totalPages: 1,
        results,
      });
    if (command === "prepare_video")
      return Promise.resolve({ filePath: "/Videos/clip-2.mp4" });
    return Promise.resolve();
  });
  const scrollTo = vi.fn();
  vi.stubGlobal("scrollTo", scrollTo);
  const focusSearchField = vi.spyOn(HTMLInputElement.prototype, "focus");
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  // How far down the list the viewer was when they picked a video out of it.
  vi.stubGlobal("scrollY", 1200);
  await user.click(
    await screen.findByRole("button", { name: "Play clip-2.mp4" }),
  );
  // What a browser does once the list unmounts: the document is only as tall as
  // the player, so the offset is clamped away. Whatever the app puts the viewer
  // back at has to have been remembered before this point.
  vi.stubGlobal("scrollY", 0);
  await user.click(
    await screen.findByRole("button", { name: "Back to results" }),
  );
  expect(
    await screen.findByRole("button", { name: "Play clip-2.mp4" }),
  ).toBeVisible();

  expect(scrollTo).toHaveBeenCalledWith(expect.objectContaining({ top: 1200 }));
  // Claiming the search field for the next search must not undo that: the field
  // is above everything the viewer scrolled past.
  expect(focusSearchField).toHaveBeenCalledWith({ preventScroll: true });
  focusSearchField.mockRestore();
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
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 25,
      totalPages: 2,
      results: firstPage,
    })
    .mockResolvedValueOnce({
      query: "clip",
      page: 2,
      pageSize: 24,
      totalResults: 25,
      totalPages: 2,
      results: [last],
    })
    .mockResolvedValue({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  const playlist = await screen.findByRole("complementary", {
    name: "Playlist",
  });
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
  expect(invokeMock).toHaveBeenLastCalledWith("prepare_video", {
    resultId: "video-1",
  });
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
  expect(
    screen.getByRole("button", { name: "playlist-1.mp4" }),
  ).toHaveAttribute("aria-current", "true");

  await user.click(toggle);
  expect(
    screen.queryByRole("complementary", { name: "Playlist" }),
  ).not.toBeInTheDocument();
  await user.click(toggle);
  await user.click(screen.getByRole("button", { name: "playlist-3.mp4" }));

  expect(await screen.findByLabelText("Playing playlist-3.mp4")).toBeVisible();
  expect(
    screen.getByRole("button", { name: "playlist-3.mp4" }),
  ).toHaveAttribute("aria-current", "true");
});

test("keeps the chosen playback speed when the playlist advances", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-1.mp4" })
    .mockResolvedValueOnce({ filePath: "/Videos/playlist-2.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  const first = await screen.findByLabelText("Playing playlist-1.mp4");
  await user.selectOptions(
    screen.getByRole("combobox", { name: "Playback speed" }),
    "1.5",
  );
  expect(first).toHaveProperty("playbackRate", 1.5);

  fireEvent.ended(first);
  const second = await screen.findByLabelText("Playing playlist-2.mp4");
  fireEvent.loadedMetadata(second);

  // Choosing a speed is a decision about the sitting, not about one file.
  expect(screen.getByRole("combobox", { name: "Playback speed" })).toHaveValue(
    "1.5",
  );
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
      return Promise.resolve({
        filePath: `/Videos/${resultId}.mp4`,
        playbackBackend: "native",
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({
        duration: 120,
        currentTime: 1,
        paused: false,
        ended: false,
      });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Playing native-1.mp4");
  await user.selectOptions(
    screen.getByRole("combobox", { name: "Playback speed" }),
    "1.5",
  );
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_speed", { speed: 1.5 }),
  );

  invokeMock.mockClear();
  await user.click(screen.getByRole("button", { name: "Next video" }));
  await screen.findByLabelText("Playing native-2.mp4");

  // mpv starts every file at 1x, so the retained speed has to be sent again.
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_speed", { speed: 1.5 }),
  );
  expect(screen.getByRole("combobox", { name: "Playback speed" })).toHaveValue(
    "1.5",
  );
});

test("stays fullscreen when the playlist advances to the next video", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `playlist-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
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
    expect(
      screen.getByRole("button", { name: "Exit fullscreen" }),
    ).toBeVisible();

    fireEvent.ended(first);
    await screen.findByLabelText("Playing playlist-2.mp4");

    // The browser drops out of fullscreen the moment the element it promoted
    // leaves the document, so the shell has to outlive the video inside it.
    expect(document.querySelector(".player-shell")).toBe(shell);
    expect(
      screen.getByRole("button", { name: "Exit fullscreen" }),
    ).toBeVisible();
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

test("hides the native surface when opening a video fails", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "broken",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [
          { id: "broken-video", fileName: "broken.mp4", extension: "mp4" },
        ],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/broken.mp4",
        playbackBackend: "native",
      });
    }
    if (command === "load_native_video") {
      return Promise.reject(new Error("The video file is empty."));
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "broken{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play broken.mp4" }),
  );

  // GTK places native video above the WebView. Once native playback reports an
  // error, leaving that surface visible makes it cover the error view (and the
  // results shown after Back) at its last, cropped position.
  expect(
    await screen.findByRole("heading", {
      name: "This video could not be played",
    }),
  ).toBeVisible();
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
      x: 0,
      y: 0,
      width: 1,
      height: 1,
      visible: false,
    }),
  );
  expect(invokeMock).toHaveBeenCalledWith("stop_native_video");
});

test("keeps the native surface hidden after playback fails mid-file", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "broken",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [
          { id: "broken-video", fileName: "broken.mp4", extension: "mp4" },
        ],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/broken.mp4",
        playbackBackend: "native",
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    // An empty file loads, so the surface is already up and cropped by the time
    // the engine admits it has nothing to show.
    if (command === "native_playback_state") {
      return Promise.reject(new Error("Rendering the video failed."));
    }
    return Promise.resolve();
  });
  const boxes = vi
    .spyOn(Element.prototype, "getBoundingClientRect")
    .mockImplementation(function (this: Element) {
      const box = (left: number, top: number, width: number, height: number) =>
        ({
          x: left,
          y: top,
          top,
          left,
          right: left + width,
          bottom: top + height,
          width,
          height,
        }) as DOMRect;
      if (this.classList.contains("native-video")) return box(0, 60, 800, 340);
      if (this.classList.contains("player-controls"))
        return box(0, 320, 800, 80);
      if (this.classList.contains("playlist-drawer"))
        return box(560, 0, 240, 400);
      return box(0, 0, 0, 0);
    });
  const user = userEvent.setup();
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "broken{Enter}");
    await user.click(
      await screen.findByRole("button", { name: "Play broken.mp4" }),
    );
    await screen.findByLabelText("Playing broken.mp4");
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 60,
        width: 560,
        height: 260,
        visible: true,
      }),
    );
    await screen.findByRole("heading", {
      name: "This video could not be played",
    });
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
        visible: false,
      }),
    );

    // The error view has no surface to measure, so anything still watching for
    // a new size would hand the hidden layer bounds of its own invention and
    // put it back over the view — the position the report describes.
    invokeMock.mockClear();
    act(() => void fireEvent(window, new Event("resize")));
    await Promise.resolve();
    expect(invokeMock).not.toHaveBeenCalledWith(
      "set_native_video_bounds",
      expect.objectContaining({ visible: true }),
    );
    // Nothing is playing any more either, so the failed engine stops being
    // asked how far along it is.
    expect(invokeMock).not.toHaveBeenCalledWith(
      "native_playback_state",
      undefined,
    );
  } finally {
    boxes.mockRestore();
  }
});

test("keeps native video clear of the player controls and playlist drawer", async () => {
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
      return Promise.resolve({
        filePath: `/Videos/${resultId}.mp4`,
        playbackBackend: "native",
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({
        duration: 120,
        currentTime: 1,
        paused: false,
        ended: false,
      });
    }
    return Promise.resolve();
  });
  // jsdom has no layout, so the player is handed the geometry a real window
  // would have: an 800x400 picture, the overlay along the bottom 80px and the
  // drawer down the right-hand 240px.
  const layout: Record<string, Partial<DOMRect>> = {
    "native-video": {
      x: 0,
      y: 0,
      top: 0,
      left: 0,
      right: 800,
      bottom: 400,
      width: 800,
      height: 400,
    },
    "player-controls": {
      x: 0,
      y: 320,
      top: 320,
      left: 0,
      right: 800,
      bottom: 400,
      width: 800,
      height: 80,
    },
    "playlist-drawer": {
      x: 560,
      y: 0,
      top: 0,
      left: 560,
      right: 800,
      bottom: 400,
      width: 240,
      height: 400,
    },
  };
  const boxes = vi
    .spyOn(Element.prototype, "getBoundingClientRect")
    .mockImplementation(function (this: Element) {
      const named = Object.keys(layout).find((name) =>
        this.classList.contains(name),
      );
      const empty = {
        x: 0,
        y: 0,
        top: 0,
        left: 0,
        right: 0,
        bottom: 0,
        width: 0,
        height: 0,
      };
      return { ...empty, ...(named ? layout[named] : {}) } as DOMRect;
    });
  const user = userEvent.setup();
  render(<App />);

  try {
    await user.type(screen.getByRole("searchbox"), "native{Enter}");
    await user.click(await screen.findByRole("button", { name: "Play all" }));
    await screen.findByLabelText("Playing native-1.mp4");

    expect(screen.getByRole("button", { name: "Playlist 2" })).toBeVisible();
    // GTK composites the mpv surface above the WebView whatever the z-index
    // says, so the controls and drawer are only visible on Linux if the
    // surface stops short of them.
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 0,
        width: 560,
        height: 320,
        visible: true,
      }),
    );

    // Closing the drawer has to hand that strip back, or the picture stays
    // cropped for the rest of playback.
    invokeMock.mockClear();
    await user.click(screen.getByRole("button", { name: "Playlist 2" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 0,
        width: 800,
        height: 320,
        visible: true,
      }),
    );
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
      return Promise.resolve({
        filePath: `/Videos/${resultId}.mp4`,
        playbackBackend: "native",
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({
        duration: 120,
        currentTime: 1,
        paused: false,
        ended: false,
      });
    }
    return Promise.resolve();
  });
  // jsdom has no layout, so a 1200x800 fullscreen window is described here: the
  // stylesheet keeps the picture 6px clear of the bottom for the scrubber, the
  // collapsed overlay is exactly that sliver, and the full overlay is 96px tall.
  const box = (left: number, top: number, width: number, height: number) =>
    ({
      x: left,
      y: top,
      top,
      left,
      right: left + width,
      bottom: top + height,
      width,
      height,
    }) as DOMRect;
  const boxes = vi
    .spyOn(Element.prototype, "getBoundingClientRect")
    .mockImplementation(function (this: Element) {
      if (this.classList.contains("native-video")) return box(0, 0, 1200, 794);
      if (this.classList.contains("player-controls")) {
        return this.classList.contains("idle")
          ? box(0, 794, 1200, 6)
          : box(0, 704, 1200, 96);
      }
      // Path and clock share one row along the bottom, so the overlay costs the
      // picture a single strip rather than one at each end.
      if (this.classList.contains("fullscreen-info"))
        return box(16, 760, 1168, 18);
      if (this.classList.contains("playlist-drawer"))
        return box(920, 0, 280, 800);
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

    // GTK composites the mpv surface above the WebView whatever the z-index
    // says, so anything the overlay draws is only seen on Linux if the surface
    // stops short of it. The controls are hidden; the path and the clock share
    // one row, so the picture keeps its whole top and stops above that row.
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 0,
        width: 1200,
        height: 760,
        visible: true,
      }),
    );

    // Turning the overlay off hands that strip back without changing the
    // selected information/scrubber mode.
    invokeMock.mockClear();
    act(() => void fireEvent.keyDown(window, { key: "i" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 0,
        width: 1200,
        height: 794,
        visible: true,
      }),
    );

    // Bringing it back reserves its single bottom row again.
    invokeMock.mockClear();
    act(() => void fireEvent.keyDown(window, { key: "i" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 0,
        width: 1200,
        height: 760,
        visible: true,
      }),
    );

    invokeMock.mockClear();
    movePointer(window.innerWidth - 1);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_native_video_bounds", {
        x: 0,
        y: 0,
        width: 920,
        height: 704,
        visible: true,
      }),
    );
    expect(
      screen.getByRole("button", { name: "Exit fullscreen" }),
    ).toBeVisible();
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
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
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
    expect(
      screen.getByRole("button", { name }).querySelector("svg.control-icon"),
    ).toBeInTheDocument();
  }
});

test("wraps playlist navigation at both ends", async () => {
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
    .mockResolvedValueOnce({
      query: "playlist",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
    .mockResolvedValue({ filePath: "/Videos/playlist-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "playlist{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Video controls");

  // A loose run of text in a flex row is an anonymous box the stylesheet cannot
  // reach, so its line box — descender space and all — is what gets centred,
  // and the label reads a few pixels above the icon beside it.
  const loose = [
    ...document.querySelectorAll<HTMLElement>(
      ".player-controls button, .back-button, .playlist-toggle",
    ),
  ]
    .filter((control) =>
      [...control.childNodes].some(
        (node) => node.nodeType === Node.TEXT_NODE && node.textContent?.trim(),
      ),
    )
    .map(
      (control) => control.getAttribute("aria-label") ?? control.textContent,
    );
  expect(loose).toEqual([]);

  for (const name of [
    "Back to results",
    "Skip back 10 seconds",
    "Skip forward 10 seconds",
    "Subtitles",
  ]) {
    expect(
      screen.getByRole("button", { name }).querySelector(".control-label"),
    ).toBeInTheDocument();
  }
  expect(
    screen
      .getByRole("button", { name: "Playlist 2" })
      .querySelector(".playlist-count .control-label"),
  ).toHaveTextContent("2");
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
      return Promise.resolve({
        filePath: `/Videos/${resultId}.mp4`,
        playbackBackend: "native",
      });
    }
    if (command === "load_native_video") {
      loaded = String((args as { filePath: string }).filePath);
      return Promise.resolve();
    }
    if (command === "native_playback_state") {
      // Only the first file has run out, so the playlist has somewhere to go
      // and the test is not racing a second advance.
      const ended = loaded.includes("native-1");
      return Promise.resolve({
        duration: 30,
        currentTime: ended ? 28 : 1,
        paused: ended,
        ended,
      });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  expect(await screen.findByLabelText("Playing native-1.mp4")).toBeVisible();
  await waitFor(() => {
    expect(invokeMock).toHaveBeenCalledWith("set_native_paused", {
      paused: false,
    });
  });
  expect(
    await screen.findByLabelText(
      "Playing native-2.mp4",
      {},
      { timeout: 1_000 },
    ),
  ).toBeVisible();
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
      return Promise.resolve({
        filePath: `/Videos/${resultId}.mp4`,
        playbackBackend: "native",
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      // mpv's last reported position falls short of the duration, which is what
      // stopped the scrubber before the end of the bar.
      return Promise.resolve({
        duration: 30,
        currentTime: ended ? 28 : 4,
        paused: ended,
        ended,
      });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));
  await screen.findByLabelText("Playing native-1.mp4");
  await waitFor(() =>
    expect(screen.getByLabelText("Video timeline")).toHaveValue("4"),
  );

  await user.click(loopControl());
  await user.click(loopControl());
  expect(loopControl()).toHaveAccessibleName("Loop: off");

  ended = true;
  await waitFor(
    () => expect(screen.getByLabelText("Video timeline")).toHaveValue("30"),
    { timeout: 2_000 },
  );
  expect(screen.getByText("0:30 / 0:30")).toBeVisible();
  // "Off" holds at the end of this video rather than starting the next one.
  expect(screen.getByLabelText("Playing native-1.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 1 of 2")).toBeVisible();
});

test("hands the search results to another player installed on this computer", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `clip-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "clip",
        page: 1,
        pageSize: 24,
        totalResults: 2,
        totalPages: 1,
        results,
      });
    }
    if (command === "open_in_external_player") return Promise.resolve(2);
    return Promise.resolve();
  });
  externalPlayersMock.mockResolvedValue([
    { command: "vlc", name: "VLC" },
    { command: "mpv", name: "mpv" },
  ]);
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");

  const chooser = await screen.findByRole("combobox", { name: "Video player" });
  expect(chooser).toHaveValue("vlc");
  await user.selectOptions(chooser, "mpv");
  await user.click(screen.getByRole("button", { name: "Open in player" }));

  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("open_in_external_player", {
      player: "mpv",
      resultIds: ["video-1", "video-2"],
    }),
  );
});

test("opens the chosen player from its keyboard shortcut", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "clip",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
      });
    }
    if (command === "open_in_external_player") return Promise.resolve(1);
    return Promise.resolve();
  });
  externalPlayersMock.mockResolvedValue([{ command: "vlc", name: "VLC" }]);
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  const open = await screen.findByRole("button", { name: "Open in player" });
  expect(open).toHaveAttribute("aria-keyshortcuts", "Ctrl+O");

  // The search field holds the keyboard on this screen, so a bare letter would
  // be typed into it rather than reaching the app.
  const field = screen.getByRole("searchbox");
  fireEvent.keyDown(field, { key: "o", ctrlKey: true });
  expect(field).toHaveValue("clip");

  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("open_in_external_player", {
      player: "vlc",
      resultIds: ["video-1"],
    }),
  );
});

// The Ubuntu launcher raises the Toka that is already running rather than
// starting another, so asking for a second one has to be possible from inside
// the first.
test("starts a second Toka from the control and from Ctrl+N", async () => {
  const user = userEvent.setup();
  render(<App />);

  const control = screen.getByRole("button", { name: "New window" });
  expect(control).toHaveAttribute("aria-keyshortcuts", "Ctrl+N");
  expect(control.querySelector(".key-hint")).toHaveTextContent("Ctrl+N");

  await user.click(control);
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("open_new_window"),
  );

  // The search field holds the keyboard on this screen, so a bare letter would
  // be typed into it rather than reaching the app.
  invokeMock.mockClear();
  const field = screen.getByRole("searchbox");
  fireEvent.keyDown(field, { key: "n", ctrlKey: true });
  expect(field).toHaveValue("");
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("open_new_window"),
  );
});

test("says why a second Toka could not be started", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "open_new_window")
      return Promise.reject(new Error("Another Toka could not be started."));
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);

  await user.click(screen.getByRole("button", { name: "New window" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Another Toka could not be started.",
  );
});

test("says nothing about other players when this computer has none", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "clip",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
      });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  expect(await screen.findByText("1 video")).toBeVisible();

  expect(
    screen.queryByRole("button", { name: "Open in player" }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("combobox", { name: "Video player" }),
  ).not.toBeInTheDocument();
});

// `autoFocus` fires once per mount, and the search form stays mounted for the
// whole session — the player renders beside it rather than replacing it. So the
// field was focused at startup and never again, and coming back from a video
// left the keyboard on a shell that had just been unmounted.
test("focuses the search field on startup and on every return from a video", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValue({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);

  const field = screen.getByRole("searchbox");
  expect(field).toHaveFocus();

  await user.type(field, "clip{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  await screen.findByLabelText("Playing clip.mp4");
  expect(field).not.toHaveFocus();

  await user.click(screen.getByRole("button", { name: "Back to results" }));
  await waitFor(() => expect(field).toHaveFocus());
  // Typing goes straight into the field, with nothing else to click first.
  await user.keyboard("second");
  expect(field).toHaveValue("clipsecond");
});

// Aspect deliberately ignores rotation: some players apply an override
// sideways once a video is turned, which makes the control unpredictable. A
// CSS aspect-ratio is a layout property and `rotate()` a post-layout
// transform, so the web path gets that for free; mpv applies
// `video-aspect-override` to the source before `video-rotate`, so does it.
test("cycles the picture through common aspect ratios and back to auto", async () => {
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 1,
      totalPages: 1,
      results: [{ id: "video-1", fileName: "clip.mp4", extension: "mp4" }],
    })
    .mockResolvedValue({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");

  const control = screen.getByRole("button", { name: "Aspect ratio: auto" });
  expect(control).toHaveAttribute("aria-keyshortcuts", "A");
  expect(video).not.toHaveStyle({ aspectRatio: "16 / 9" });
  // The shape the picture is in is written on the control, the way the skip
  // step is, so cycling says what it landed on rather than only what it did.
  expect(control).toHaveTextContent("auto");

  await user.click(control);
  expect(
    screen.getByRole("button", { name: "Aspect ratio: 16:9" }),
  ).toHaveTextContent("16:9");
  expect(video).toHaveStyle({ aspectRatio: "16 / 9", objectFit: "fill" });

  // Rotating must not change which way the override is applied.
  await user.click(screen.getByRole("button", { name: "Rotate right" }));
  expect(video).toHaveStyle({
    aspectRatio: "16 / 9",
    transform: "rotate(90deg)",
  });

  // The rest of the cycle, each stop naming itself: the tall shapes a phone
  // records in, then the wide ones a film is cut for.
  const rest = [
    ["4:3", "4 / 3"],
    ["1:1", "1 / 1"],
    ["3:4", "3 / 4"],
    ["9:16", "9 / 16"],
    ["21:9", "21 / 9"],
    ["2.39:1", "2.39 / 1"],
  ];
  for (const [label, ratio] of rest) {
    fireEvent.keyDown(window, { key: "a" });
    expect(
      screen.getByRole("button", { name: `Aspect ratio: ${label}` }),
    ).toHaveTextContent(label);
    expect(video).toHaveStyle({ aspectRatio: ratio });
  }

  // Back where it started, with the browser choosing the shape again.
  fireEvent.keyDown(window, { key: "a" });
  expect(
    screen.getByRole("button", { name: "Aspect ratio: auto" }),
  ).toHaveTextContent("auto");
  expect(video).not.toHaveStyle({ objectFit: "fill" });
});

test("sends the chosen aspect ratio to native playback", async () => {
  invokeMock.mockImplementation((command: string) => {
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [{ id: "video-1", fileName: "native.mkv", extension: "mkv" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/native.mkv",
        playbackBackend: "native",
        subtitles: [],
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({
        duration: 120,
        currentTime: 1,
        paused: false,
        ended: false,
      });
    }
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play native.mkv" }),
  );
  await screen.findByLabelText("Playing native.mkv");

  await user.click(screen.getByRole("button", { name: "Aspect ratio: auto" }));
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_video_aspect", {
      ratio: 16 / 9,
    }),
  );

  // The decimal shape reaches mpv as the number it stands for rather than as
  // the text it is written with.
  for (let step = 0; step < 6; step += 1)
    fireEvent.keyDown(window, { key: "a" });
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("set_native_video_aspect", {
      ratio: 2.39,
    }),
  );

  // Auto hands the shape back to mpv rather than pinning it to the last value.
  fireEvent.keyDown(window, { key: "a" });
  await waitFor(() => {
    const ratios = invokeMock.mock.calls
      .filter(([command]) => command === "set_native_video_aspect")
      .map(([, args]) => (args as { ratio: number }).ratio);
    expect(ratios.at(-1)).toBe(-1);
  });
});

// Ten seconds is a compromise: too far for finding the frame a face appears
// on, too short for walking through an hour of footage. The step is therefore
// a choice of its own, and whatever is chosen has to move both skips.
test("cycles the skip step and applies it to both directions", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 3600 });
  Object.defineProperty(video, "currentTime", {
    configurable: true,
    writable: true,
    value: 1000,
  });
  fireEvent.timeUpdate(video);

  const control = screen.getByRole("button", { name: "Skip step: 10 seconds" });
  expect(control).toHaveAttribute("aria-keyshortcuts", "J");
  expect(control.querySelector(".key-hint")).toHaveTextContent("J");

  // The step it starts on is the one both skips used before they could be
  // changed.
  fireEvent.keyDown(window, { key: "ArrowRight" });
  expect(video).toHaveProperty("currentTime", 1010);

  await user.click(control);
  expect(
    screen.getByRole("button", { name: "Skip step: 30 seconds" }),
  ).toBeVisible();
  fireEvent.keyDown(window, { key: "ArrowRight" });
  expect(video).toHaveProperty("currentTime", 1040);
  // Backwards moves by exactly what forwards did.
  await user.click(
    screen.getByRole("button", { name: "Skip back 30 seconds" }),
  );
  expect(video).toHaveProperty("currentTime", 1010);

  fireEvent.keyDown(window, { key: "j" });
  await user.click(
    screen.getByRole("button", { name: "Skip forward 1 minute" }),
  );
  expect(video).toHaveProperty("currentTime", 1070);

  for (const name of ["5 minutes", "10 minutes"]) {
    fireEvent.keyDown(window, { key: "j" });
    expect(
      screen.getByRole("button", { name: `Skip step: ${name}` }),
    ).toBeVisible();
  }

  // Round the cycle: the frame and the short steps come next, and it wraps
  // back to where it started rather than sticking at the far end.
  for (const name of [
    "1 frame (assumed 30 fps)",
    "1 second",
    "5 seconds",
    "10 seconds",
  ]) {
    fireEvent.keyDown(window, { key: "j" });
    expect(
      screen.getByRole("button", { name: `Skip step: ${name}` }),
    ).toBeVisible();
  }
  fireEvent.keyDown(window, { key: "ArrowLeft" });
  expect(video).toHaveProperty("currentTime", 1060);
});

// Ten seconds is a nudge through a feature and a third of a short clip, so the
// step each video starts on is read from that video's own length.
test("starts each video on the skip step closest to a fifth of its length", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `clip-${number}.mp4`,
    extension: "mp4",
  }));
  invokeMock
    .mockResolvedValueOnce({
      query: "clip",
      page: 1,
      pageSize: 24,
      totalResults: 2,
      totalPages: 1,
      results,
    })
    .mockResolvedValue({ filePath: "/Videos/clip.mp4" });
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "clip{Enter}");
  await user.click(await screen.findByRole("button", { name: "Play all" }));

  // Fifty minutes: a fifth of it is exactly one of the steps on offer.
  const first = await screen.findByLabelText("Playing clip-1.mp4");
  Object.defineProperty(first, "duration", {
    configurable: true,
    value: 3000,
  });
  fireEvent.loadedMetadata(first);
  expect(
    await screen.findByRole("button", { name: "Skip step: 10 minutes" }),
  ).toBeVisible();

  // The next video is read on its own terms rather than inheriting the last
  // one's step: two minutes wants thirty seconds, the closest step to twenty-
  // four.
  fireEvent.keyDown(window, { key: "PageDown" });
  const second = await screen.findByLabelText("Playing clip-2.mp4");
  Object.defineProperty(second, "duration", { configurable: true, value: 120 });
  fireEvent.loadedMetadata(second);
  expect(
    await screen.findByRole("button", { name: "Skip step: 30 seconds" }),
  ).toBeVisible();
});

// The length arrives a moment after the file opens, and on the native path a
// poll keeps repeating it. Neither may undo a choice the viewer has already
// made about the file they are watching.
test("leaves a chosen skip step alone when the length arrives late", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");

  fireEvent.keyDown(window, { key: "j" });
  expect(
    screen.getByRole("button", { name: "Skip step: 30 seconds" }),
  ).toBeVisible();

  Object.defineProperty(video, "duration", { configurable: true, value: 3000 });
  fireEvent.loadedMetadata(video);
  expect(
    await screen.findByRole("button", { name: "Skip step: 30 seconds" }),
  ).toBeVisible();
});

// A `<video>` element never says how fast its frames run, so the web path can
// only assume — and the control says so rather than presenting a guess as a
// measurement.
test("steps an assumed frame on the web backend and names the assumption", async () => {
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
  await user.click(
    await screen.findByRole("button", { name: "Play clip.mp4" }),
  );
  const video = await screen.findByLabelText("Playing clip.mp4");
  Object.defineProperty(video, "duration", { configurable: true, value: 120 });
  Object.defineProperty(video, "currentTime", {
    configurable: true,
    writable: true,
    value: 20,
  });
  fireEvent.timeUpdate(video);

  await user.click(
    screen.getByRole("button", { name: "Skip step: 10 seconds" }),
  );
  for (let press = 0; press < 4; press += 1)
    fireEvent.keyDown(window, { key: "j" });
  expect(
    screen.getByRole("button", { name: "Skip step: 1 frame (assumed 30 fps)" }),
  ).toBeVisible();

  fireEvent.keyDown(window, { key: "ArrowRight" });
  expect((video as HTMLVideoElement).currentTime).toBeCloseTo(20 + 1 / 30, 5);
  fireEvent.keyDown(window, { key: "ArrowLeft" });
  expect((video as HTMLVideoElement).currentTime).toBeCloseTo(20, 5);
});

// mpv knows the real rate once the file is open, so on that backend a frame is
// a frame rather than a thirtieth of a second.
test("steps a real frame using the rate native playback reports", async () => {
  // mpv answers with wherever the last seek left the file, so the polling that
  // follows a skip reports the position that skip asked for.
  let nativeTime = 10;
  invokeMock.mockImplementation((command: string, args?: unknown) => {
    if (command === "seek_native_video") {
      nativeTime = (args as { seconds: number }).seconds;
      return Promise.resolve();
    }
    if (command === "search_videos") {
      return Promise.resolve({
        query: "native",
        page: 1,
        pageSize: 24,
        totalResults: 1,
        totalPages: 1,
        results: [{ id: "video-1", fileName: "native.mkv", extension: "mkv" }],
      });
    }
    if (command === "prepare_video") {
      return Promise.resolve({
        filePath: "/Videos/native.mkv",
        playbackBackend: "native",
        subtitles: [],
      });
    }
    if (command === "native_video_rotation") return Promise.resolve(0);
    if (command === "native_playback_state") {
      return Promise.resolve({
        duration: 120,
        currentTime: nativeTime,
        paused: false,
        ended: false,
        frameRate: 25,
      });
    }
    return Promise.resolve();
  });
  const seeks = () =>
    invokeMock.mock.calls
      .filter(([command]) => command === "seek_native_video")
      .map(([, args]) => (args as { seconds: number }).seconds);
  const user = userEvent.setup();
  render(<App />);
  await user.type(screen.getByRole("searchbox"), "native{Enter}");
  await user.click(
    await screen.findByRole("button", { name: "Play native.mkv" }),
  );
  await screen.findByLabelText("Playing native.mkv");

  // Two minutes, so the poll that reports the length settles the step on the
  // thirty seconds nearest a fifth of it. Waiting for that rather than for the
  // opening step keeps the cycle below counting from a known place.
  await waitFor(() =>
    expect(
      screen.getByRole("button", { name: "Skip step: 30 seconds" }),
    ).toBeVisible(),
  );
  for (let press = 0; press < 4; press += 1)
    fireEvent.keyDown(window, { key: "j" });
  await waitFor(() =>
    expect(
      screen.getByRole("button", { name: "Skip step: 1 frame (25 fps)" }),
    ).toBeVisible(),
  );

  fireEvent.keyDown(window, { key: "ArrowRight" });
  await waitFor(() => expect(seeks().at(-1)).toBeCloseTo(10 + 1 / 25, 5));

  // Backwards takes whichever step is chosen, from where the last one landed.
  fireEvent.keyDown(window, { key: "j" });
  await waitFor(() =>
    expect(
      screen.getByRole("button", { name: "Skip step: 1 second" }),
    ).toBeVisible(),
  );
  fireEvent.keyDown(window, { key: "ArrowLeft" });
  await waitFor(() => expect(seeks().at(-1)).toBeCloseTo(10 + 1 / 25 - 1, 5));
});

test("plays the playlist Toka was launched with and leaves it to come back to", async () => {
  const results = [1, 2].map((number) => ({
    id: `video-${number}`,
    fileName: `launched-${number}.mp4`,
    extension: "mp4",
  }));
  launchPlaylistMock.mockResolvedValue({
    query: "summer.m3u8",
    page: 1,
    pageSize: 24,
    totalResults: 2,
    totalPages: 1,
    results,
  });
  invokeMock.mockResolvedValue({ filePath: "/Videos/launched-1.mp4" });
  const user = userEvent.setup();
  render(<App />);

  // The playlist plays on its own, from its first entry, without anything being
  // searched for or clicked.
  expect(await screen.findByLabelText("Playing launched-1.mp4")).toBeVisible();
  expect(screen.getByText("Playlist video 1 of 2")).toBeVisible();
  expect(screen.getByRole("button", { name: "Playlist 2" })).toBeVisible();
  expect(invokeMock.mock.calls.map(([command]) => command)).not.toContain(
    "search_videos",
  );

  // And its entries are the list a viewer coming out of the player lands on,
  // the same as a search's results would be.
  await user.click(screen.getByRole("button", { name: "Back to results" }));
  expect(
    screen.getByRole("button", { name: "Play launched-2.mp4" }),
  ).toBeVisible();
});

test("reports a launch playlist it cannot play and leaves the search ready", async () => {
  launchPlaylistMock.mockRejectedValue({
    kind: "Playlist",
    message: "summer.m3u8 lists no videos Toka can play.",
  });
  render(<App />);

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "summer.m3u8 lists no videos Toka can play.",
  );
  expect(screen.getByRole("searchbox")).toHaveFocus();
});
