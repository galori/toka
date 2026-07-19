import { invoke } from "@tauri-apps/api/core";

export const PAGE_SIZE = 24 as const;

export type VideoResult = {
  id: string;
  fileName: string;
  extension: string;
};

export type SearchPage = {
  query: string;
  page: number;
  pageSize: typeof PAGE_SIZE;
  totalResults: number;
  totalPages: number;
  results: VideoResult[];
};

export function searchVideos(query: string, page: number): Promise<SearchPage> {
  return invoke<SearchPage>("search_videos", {
    request: { query, page, pageSize: PAGE_SIZE },
  });
}

export function prepareVideo(resultId: string): Promise<{ filePath: string }> {
  return invoke("prepare_video", { resultId });
}
