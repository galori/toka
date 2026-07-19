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
