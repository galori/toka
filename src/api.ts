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

export type SidecarSubtitle = {
  track: number;
  label: string;
  language: string | null;
  webPlayable: boolean;
};

export type PreparedVideo = {
  filePath: string;
  playbackBackend: "native" | "web";
  subtitles?: SidecarSubtitle[];
};

// WebVTT for a sidecar subtitle. Rust reads the file so the frontend never
// handles a filesystem path.
export function subtitleCues(resultId: string, track: number): Promise<string> {
  return invoke("subtitle_cues", { resultId, track });
}

export type NativeSubtitleTrack = {
  id: number;
  label: string;
  external: boolean;
};

export function nativeSubtitleTracks(): Promise<NativeSubtitleTrack[]> {
  return invoke("native_subtitle_tracks");
}

// `null` turns subtitles off.
export function setNativeSubtitle(id: number | null): Promise<void> {
  return invoke("set_native_subtitle", { id });
}

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
export function setNativeSpeed(speed: number): Promise<void> { return invoke("set_native_speed", { speed }); }

export function nativeVideoRotation(): Promise<number> {
  return invoke("native_video_rotation");
}

export function setNativeVideoRotation(degrees: number): Promise<void> {
  return invoke("set_native_video_rotation", { degrees });
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
