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

export type PreparedVideo = {
  filePath: string;
  playbackBackend: "native" | "web";
};

export type PlaybackState = {
  duration: number;
  currentTime: number;
  paused: boolean;
  ended: boolean;
};

export function prepareVideo(resultId: string): Promise<PreparedVideo> {
  return invoke("prepare_video", { resultId });
}

export function loadNativeVideo(filePath: string): Promise<void> {
  return invoke("load_native_video", { filePath });
}

export function setNativePaused(paused: boolean): Promise<void> {
  return invoke("set_native_paused", { paused });
}

export function seekNativeVideo(seconds: number): Promise<void> {
  return invoke("seek_native_video", { seconds });
}

export function nativePlaybackState(): Promise<PlaybackState> {
  return invoke("native_playback_state");
}

export function stopNativeVideo(): Promise<void> {
  return invoke("stop_native_video");
}

export function setNativeVideoBounds(bounds: {
  x: number;
  y: number;
  width: number;
  height: number;
  visible: boolean;
}): Promise<void> {
  return invoke("set_native_video_bounds", bounds);
}
