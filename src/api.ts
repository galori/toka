import { invoke } from "@tauri-apps/api/core";

export const PAGE_SIZE = 24 as const;

export type VideoResult = {
  id: string;
  fileName: string;
  extension: string;
  thumbnailPath?: string;
  tags?: string[];
};

export type VideoTagUpdate = Pick<VideoResult, "fileName" | "tags">;

export type SearchPage = {
  query: string;
  page: number;
  pageSize: typeof PAGE_SIZE;
  totalResults: number;
  totalPages: number;
  indexRevision?: number;
  results: VideoResult[];
};

// Which part of a video a query is matched against. The three are separate
// haystacks rather than nested ones: the tag block belongs to the tags, the
// file name is read without it, and the path is the folders above the file.
export type SearchFields = {
  tags: boolean;
  fileName: boolean;
  path: boolean;
};

// What a search covered before the parts could be chosen: the whole file name,
// tag block included, and nothing of the folders above it.
export const DEFAULT_SEARCH_FIELDS: SearchFields = {
  tags: true,
  fileName: true,
  path: false,
};

export type MediaType = "videos" | "images" | "both";
export const DEFAULT_MEDIA_TYPE: MediaType = "videos";

export type IndexFolder = {
  id: string;
  path: string;
  status: "pending" | "indexing" | "ready" | "offline" | "error";
  message?: string;
};

export type IndexState = {
  supported: boolean;
  revision: number;
  folders: IndexFolder[];
};

export function indexState(): Promise<IndexState> {
  return invoke<IndexState>("index_state");
}

export function addIndexFolder(path: string): Promise<IndexState> {
  return invoke<IndexState>("add_index_folder", { path });
}

export function removeIndexFolder(id: string): Promise<IndexState> {
  return invoke<IndexState>("remove_index_folder", { id });
}

export function searchVideos(
  query: string,
  page: number,
  fields: SearchFields = DEFAULT_SEARCH_FIELDS,
  mediaType: MediaType = DEFAULT_MEDIA_TYPE,
): Promise<SearchPage> {
  return invoke<SearchPage>("search_videos", {
    request: { query, page, pageSize: PAGE_SIZE, fields, mediaType },
  });
}

// The playlist file Toka was launched with — `toka summer.m3u8`, or a playlist
// opened from a file manager — as a page of results to play. `null` is the
// ordinary case: Toka was started on its own. A playlist that cannot be read,
// or that has nothing left in it to play, rejects with the reason.
export function launchPlaylist(): Promise<SearchPage | null> {
  return invoke<SearchPage | null>("launch_playlist");
}

export function videoThumbnail(resultId: string): Promise<string> {
  return invoke<string>("video_thumbnail", { resultId });
}

// Frames sampled across the video, to run through while the pointer rests on a
// result. Asked for on hover rather than with the search: making them is
// several seeks through the file, and most results are never pointed at.
export function videoPreview(resultId: string): Promise<string[]> {
  return invoke<string[]>("video_preview", { resultId });
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
  // How fast the open file's frames run, so a one-frame skip can be a frame
  // rather than a guess. Zero until mpv has read a rate out of the file.
  frameRate?: number;
};

export function prepareVideo(resultId: string): Promise<PreparedVideo> {
  return invoke("prepare_video", { resultId });
}

export function deleteVideo(resultId: string): Promise<void> {
  return invoke("delete_video", { resultId });
}
export function undoDelete(): Promise<void> {
  return invoke("undo_delete");
}

// A video player installed on this computer that a search can be handed to.
export type ExternalPlayer = { command: string; name: string };

export function externalPlayers(): Promise<ExternalPlayer[]> {
  return invoke("external_players");
}

// Writes the results to a playlist file, opens it in `player`, and answers with
// how many videos it handed over.
export function openInExternalPlayer(
  player: string,
  resultIds: string[],
): Promise<number> {
  return invoke("open_in_external_player", { player, resultIds });
}

// Replaces the whole tag set; an empty list clears every tag.
export function setVideoTags(
  resultId: string,
  tags: string[],
): Promise<VideoTagUpdate> {
  return invoke("set_video_tags", { resultId, tags });
}

export function addVideoTags(
  resultId: string,
  tags: string[],
): Promise<VideoTagUpdate> {
  return invoke("add_video_tags", { resultId, tags });
}

// One video out of a page tagged in a single go. Tagging renames the file, so
// the id is what the update is matched back to — never the name.
export type TaggedVideo = VideoTagUpdate & { resultId: string };

export type BulkTagUpdate = {
  tagged: TaggedVideo[];
  failed: number;
  // Why the first video that could not be tagged was left alone.
  problem?: string;
};

// Puts the same tags on every video at once. A page of results is hundreds of
// videos and each tag is a rename, so this is one request rather than one per
// video — and it tags what it can rather than stopping at the first failure.
export function addTagsToVideos(
  resultIds: string[],
  tags: string[],
): Promise<BulkTagUpdate> {
  return invoke("add_tags_to_videos", { resultIds, tags });
}

export function removeVideoTags(
  resultId: string,
  tags: string[],
): Promise<VideoTagUpdate> {
  return invoke("remove_video_tags", { resultId, tags });
}

export function loadNativeVideo(filePath: string): Promise<void> {
  return invoke("load_native_video", { filePath });
}

export function setNativePaused(paused: boolean): Promise<void> {
  return invoke("set_native_paused", { paused });
}
export function setNativeSpeed(speed: number): Promise<void> {
  return invoke("set_native_speed", { speed });
}

export function setNativeVolume(volume: number): Promise<void> {
  return invoke("set_native_volume", { volume });
}

export function nativeVideoRotation(): Promise<number> {
  return invoke("native_video_rotation");
}

// A negative ratio hands the picture's shape back to mpv.
export function setNativeVideoAspect(ratio: number): Promise<void> {
  return invoke("set_native_video_aspect", { ratio });
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

export function openNewWindow(): Promise<void> {
  return invoke("open_new_window");
}
