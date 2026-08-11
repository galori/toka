import {
  ButtonHTMLAttributes,
  CSSProperties,
  FormEvent,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import {
  addIndexFolder,
  launchPlaylist,
  loadNativeVideo,
  nativePlaybackState,
  nativeSubtitleTracks,
  nativeVideoRotation,
  prepareVideo,
  searchVideos,
  seekNativeVideo,
  setNativePaused,
  setNativeSpeed,
  setNativeVolume,
  setNativeSubtitle,
  setNativeVideoRotation,
  setNativeVideoAspect,
  setNativeVideoBounds,
  stopNativeVideo,
  subtitleCues,
  deleteVideo,
  undoDelete,
  externalPlayers,
  indexState,
  removeIndexFolder,
  openInExternalPlayer,
  openNewWindow,
  addVideoTags,
  addTagsToVideos,
  removeVideoTags,
  videoPreview,
  videoThumbnail,
  DEFAULT_SEARCH_FIELDS,
  DEFAULT_MEDIA_TYPE,
  type BulkTagUpdate,
  type ExternalPlayer,
  type MediaType,
  type PreparedVideo,
  type SearchFields,
  type IndexState,
  type SearchPage,
  type VideoResult,
  type VideoTagUpdate,
} from "./api";
import { buildInfo } from "./buildInfo";

const IMAGE_EXTENSIONS = new Set([
  "jpg",
  "jpeg",
  "png",
  "webp",
  "gif",
  "bmp",
  "tiff",
  "tif",
  "heic",
  "heif",
  "avif",
]);

export function isImageExtension(extension: string): boolean {
  return IMAGE_EXTENSIONS.has(extension.toLowerCase());
}

export function isImageResult(result: VideoResult): boolean {
  return isImageExtension(result.extension);
}

export const SLIDESHOW_INTERVAL = 3_000;
const IMAGE_DURATION_SECONDS = SLIDESHOW_INTERVAL / 1_000;
const IMAGE_PROGRESS_TICK = 100;

const MEDIA_TYPES: { value: MediaType; label: string; shortcut: string }[] = [
  { value: "videos", label: "Videos", shortcut: "Ctrl+1" },
  { value: "images", label: "Images", shortcut: "Ctrl+2" },
  { value: "both", label: "Both", shortcut: "Ctrl+3" },
];

// A subtitle Toka can turn on, whichever backend supplies it: a sidecar file
// detected by Rust, an mpv track, or a track the web engine found in the file.
type SubtitleOption =
  | { source: "sidecar"; label: string; language: string | null; track: number }
  | { source: "native"; label: string; id: number }
  | { source: "embedded"; label: string; textTrack: TextTrack };

type ParsedCue = { startTime: number; endTime: number; text: string };

function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error && typeof error === "object" && "message" in error) {
    return String(error.message);
  }
  return "Something went wrong while searching for videos.";
}

function parseWebVttTimestamp(timestamp: string): number | undefined {
  const parts = timestamp.trim().split(":");
  if (parts.length < 2 || parts.length > 3) return undefined;
  const seconds = Number(parts.at(-1));
  const minutes = Number(parts.at(-2));
  const hours = parts.length === 3 ? Number(parts[0]) : 0;
  if (![hours, minutes, seconds].every(Number.isFinite)) return undefined;
  return hours * 3600 + minutes * 60 + seconds;
}

function parseWebVttCues(source: string): ParsedCue[] {
  const lines = source.replace(/^\uFEFF/, "").split(/\r?\n/);
  const cues: ParsedCue[] = [];
  let position = lines[0]?.trim() === "WEBVTT" ? 1 : 0;

  while (position < lines.length) {
    while (position < lines.length && lines[position].trim() === "")
      position += 1;
    if (position >= lines.length) break;

    let timing = lines[position].trim();
    if (!timing.includes("-->")) {
      position += 1;
      timing = lines[position]?.trim() ?? "";
    }

    const [start, endWithSettings] = timing.split(/\s+-->\s+/, 2);
    const end = endWithSettings?.split(/\s+/, 1)[0];
    const startTime = parseWebVttTimestamp(start ?? "");
    const endTime = parseWebVttTimestamp(end ?? "");
    position += 1;

    const text: string[] = [];
    while (position < lines.length && lines[position].trim() !== "") {
      text.push(lines[position]);
      position += 1;
    }

    if (startTime !== undefined && endTime !== undefined && text.length > 0) {
      cues.push({ startTime, endTime, text: text.join("\n") });
    }
  }

  return cues;
}

function VideoIcon() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden="true" className="video-icon">
      <rect x="7" y="12" width="50" height="40" rx="8" />
      <path d="m27 23 16 9-16 9Z" />
    </svg>
  );
}

function ImageIcon() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden="true" className="image-icon">
      <rect x="7" y="10" width="50" height="44" rx="8" />
      <circle cx="23" cy="24" r="5" />
      <path d="m12 49 15-16 9 9 6-7 10 14" />
    </svg>
  );
}

// How long the pointer has to rest on a result before its preview is made.
// Sweeping the pointer across a grid of results should not set eight seeks
// through a file going for every tile it happens to cross.
const PREVIEW_DELAY = 350;

// How long each frame of a preview is held. Slow enough to read what is in the
// picture, quick enough that a whole video goes by in a few seconds.
const PREVIEW_FRAME = 450;

function VideoThumbnail({
  video,
  previewing,
}: {
  video: VideoResult;
  previewing: boolean;
}) {
  const container = useRef<HTMLSpanElement>(null);
  const [thumbnailPath, setThumbnailPath] = useState(video.thumbnailPath);
  const [frames, setFrames] = useState<string[]>();
  const [frame, setFrame] = useState(0);
  const image = isImageResult(video);

  // Asked for once per video and then kept: the frames are files on disk that
  // do not change, so pointing at the same result again costs nothing.
  useEffect(() => {
    if (!previewing || frames) return;
    const timer = setTimeout(() => {
      void videoPreview(video.id)
        .then(setFrames)
        // A video ffmpeg cannot read frames out of keeps its still. There is
        // nothing to tell the viewer here: they pointed at a picture and the
        // picture stayed.
        .catch(() => {});
    }, PREVIEW_DELAY);
    return () => clearTimeout(timer);
  }, [previewing, frames, video.id]);

  useEffect(() => {
    if (!previewing || !frames?.length) return;
    const timer = setInterval(
      () => setFrame((current) => (current + 1) % frames.length),
      PREVIEW_FRAME,
    );
    return () => clearInterval(timer);
  }, [previewing, frames]);

  // Every visit to a result starts its preview from the beginning rather than
  // resuming wherever the last one was interrupted.
  useEffect(() => {
    if (!previewing) setFrame(0);
  }, [previewing]);

  useEffect(() => {
    if (
      thumbnailPath ||
      !container.current ||
      typeof IntersectionObserver === "undefined"
    )
      return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (!entries.some((entry) => entry.isIntersecting)) return;
        observer.disconnect();
        void videoThumbnail(video.id)
          .then(setThumbnailPath)
          .catch(() => {});
      },
      { rootMargin: "200px" },
    );
    observer.observe(container.current);
    return () => observer.disconnect();
  }, [thumbnailPath, video.id]);

  const showing = (previewing ? frames?.[frame] : undefined) ?? thumbnailPath;

  return (
    <span
      ref={container}
      className="video-art"
      style={
        showing
          ? { backgroundImage: `url(${convertFileSrc(showing)})` }
          : undefined
      }
    >
      {showing ? (
        <span className="thumbnail-overlay" aria-hidden="true" />
      ) : image ? (
        <ImageIcon />
      ) : (
        <VideoIcon />
      )}
      <span
        className={`media-type-badge ${image ? "image" : "video"}`}
        aria-hidden="true"
      >
        {image ? "Image" : "Video"}
      </span>
    </span>
  );
}

// The pointer resting on a result, or the keyboard reaching it, runs a preview
// of the video in place of its still. Both belong to the tile rather than to
// the page: a keystroke among a grid of results cannot say which of them it
// means, which is why there is no shortcut for this — the same reason the tag
// control on a result carries none.
function VideoTile({
  video,
  onPlay,
}: {
  video: VideoResult;
  onPlay: () => void;
}) {
  const [previewing, setPreviewing] = useState(false);
  return (
    <button
      type="button"
      className="video-tile"
      aria-label={`Play ${video.fileName}`}
      title={`Play ${video.fileName}`}
      onClick={onPlay}
      onPointerEnter={() => setPreviewing(true)}
      onPointerLeave={() => setPreviewing(false)}
      onFocus={() => setPreviewing(true)}
      onBlur={() => setPreviewing(false)}
    >
      <VideoThumbnail video={video} previewing={previewing} />
      <span className="video-name">{video.fileName}</span>
    </button>
  );
}

// A drawn cross rather than a "×" character: the glyph sits on the text
// baseline, which left it riding above the middle of the tag beside it, and its
// side bearings are too small to separate it from the name on their own.
function TagRemoveIcon() {
  return (
    <svg viewBox="0 0 12 12" aria-hidden="true" className="tag-remove-glyph">
      <path d="M3 3 9 9" />
      <path d="M9 3 3 9" />
    </svg>
  );
}

function VideoTags({
  video,
  onChange,
  adding: controlledAdding,
  onAddingChange,
  shortcut,
}: {
  video: VideoResult;
  onChange: (update: Pick<VideoResult, "fileName" | "tags">) => void;
  adding?: boolean;
  onAddingChange?: (adding: boolean) => void;
  shortcut?: string;
}) {
  const field = useRef<HTMLInputElement>(null);
  const [draft, setDraft] = useState("");
  const [uncontrolledAdding, setUncontrolledAdding] = useState(false);
  const [error, setError] = useState<string>();
  const adding = controlledAdding ?? uncontrolledAdding;
  const setAdding = (next: boolean) => {
    setUncontrolledAdding(next);
    onAddingChange?.(next);
  };
  const tags = video.tags ?? [];
  // Claimed on every open rather than through `autoFocus`, which fires once per
  // mount and so loses the field to whatever the fullscreen transition focuses.
  useEffect(() => {
    if (adding) field.current?.focus();
  }, [adding]);
  const save = async (update: Promise<VideoTagUpdate>) => {
    try {
      onChange(await update);
      setError(undefined);
    } catch (reason) {
      setError(`Could not update tags: ${errorMessage(reason)}`);
    }
  };
  const add = () => {
    const tag = draft.trim();
    if (!tag || tags.includes(tag)) return;
    void save(addVideoTags(video.id, [tag]));
    setDraft("");
    setAdding(false);
  };
  return (
    <div className="video-tags" aria-label={`Tags for ${video.fileName}`}>
      {tags.map((tag) => (
        <button
          key={tag}
          type="button"
          className="tag-pill"
          aria-label={`Remove tag ${tag}`}
          onClick={() => void save(removeVideoTags(video.id, [tag]))}
        >
          <span className="tag-name">{tag}</span>
          <TagRemoveIcon />
        </button>
      ))}
      {adding ? (
        <input
          ref={field}
          aria-label={`Add tag to ${video.fileName}`}
          value={draft}
          onChange={(event) => setDraft(event.currentTarget.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") add();
            if (event.key === "Escape") setAdding(false);
          }}
        />
      ) : shortcut ? (
        <ControlButton
          shortcut={shortcut}
          className="tag-add"
          aria-label={`Add tag to ${video.fileName}`}
          onClick={() => setAdding(true)}
        >
          <Label>+</Label>
        </ControlButton>
      ) : (
        // No shortcut among the results: a keystroke there cannot say which of
        // the videos on screen it means. The control stays, for the pointer.
        <button
          type="button"
          className="tag-add"
          title={`Add tag to ${video.fileName}`}
          aria-label={`Add tag to ${video.fileName}`}
          onClick={() => setAdding(true)}
        >
          <Label>+</Label>
        </button>
      )}
      {error ? (
        <p className="tag-error" role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}

function SearchIcon() {
  return (
    <svg viewBox="0 0 24 24" className="search-glyph" aria-hidden="true">
      <circle cx="10.75" cy="10.75" r="6.25" />
      <path d="m16 16 4 4" />
    </svg>
  );
}

function ThumbnailSizeIcon({ larger }: { larger: boolean }) {
  return (
    <svg
      viewBox="0 0 24 24"
      aria-hidden="true"
      className={`control-icon thumbnail-size-icon ${
        larger ? "thumbnail-size-larger" : "thumbnail-size-smaller"
      }`}
    >
      <circle cx="10.5" cy="10.5" r="6.25" />
      <path d="m15.5 15.5 4.5 4.5" />
      <path d="M7.5 10.5h6" />
      {larger ? <path d="M10.5 7.5v6" /> : null}
    </svg>
  );
}

// Text-glyph icons (⏮ ⌕ ↶ …) fall back to whichever font a Linux system has
// installed for that codepoint, which can render far smaller and thinner
// than the same character on macOS. Drawing these as SVG keeps their size
// and weight identical everywhere. The skip icons are filled rather than
// stroked on top of that: a 2-unit stroke in a 24-unit viewBox lands on less
// than a device pixel here, which WebKitGTK renders as a grey smear beside the
// solid triangle it sits next to.
function PreviousIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon solid">
      <polygon points="21 2 8 12 21 22 21 2" />
      <rect x="3" y="2" width="3.4" height="20" rx="1.7" />
    </svg>
  );
}

function NextIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon solid">
      <polygon points="3 2 16 12 3 22 3 2" />
      <rect x="17.6" y="2" width="3.4" height="20" rx="1.7" />
    </svg>
  );
}

// The set reads as one scale rather than unrelated pictures: the same speaker
// throughout, crossed out for silence, carrying one wave for quieter and both
// for louder, so what each button does is legible without reading a word.
function SpeakerIcon({ waves }: { waves: 0 | 1 | 2 }) {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <path d="M3.5 9.5h3.6L11.5 5.6v12.8L7.1 14.5H3.5z" />
      {waves === 0 ? (
        <>
          <path d="m15.6 9.6 5 4.8" />
          <path d="m20.6 9.6-5 4.8" />
        </>
      ) : (
        <path d="M15.4 10.1a4.2 4.2 0 0 1 0 3.8" />
      )}
      {waves === 2 ? <path d="M18.4 7.4a8.6 8.6 0 0 1 0 9.2" /> : null}
    </svg>
  );
}

function RotateLeftIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <polyline points="1 4 1 10 7 10" />
      <path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10" />
    </svg>
  );
}

function RotateRightIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <polyline points="23 4 23 10 17 10" />
      <path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10" />
    </svg>
  );
}

function LoopIcon({ single = false }: { single?: boolean }) {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <polyline points="17 1 21 5 17 9" />
      <path d="M3 11V9a4 4 0 0 1 4-4h14" />
      <polyline points="7 23 3 19 7 15" />
      <path d="M21 13v2a4 4 0 0 1-4 4H3" />
      {/* VLC marks "repeat this one video" with a 1 drawn between the arrows.
          Filled rather than stroked: a digit this small stroked at the icon's
          weight closes up, and a thinner stroke lands under a device pixel and
          is resampled into a grey smudge by WebKitGTK. */}
      {single ? (
        <polygon
          className="loop-one"
          points="10.4 9.9 13.2 8.2 14.2 8.2 14.2 15.8 12.4 15.8 12.4 10.5 11.2 11.2"
        />
      ) : null}
    </svg>
  );
}

function FullscreenEnterIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <path d="M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3" />
    </svg>
  );
}

function FullscreenExitIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <path d="M8 3v3a2 2 0 0 1-2 2H3m18 0h-3a2 2 0 0 1-2-2V3m0 18v-3a2 2 0 0 1 2-2h3M3 16h3a2 2 0 0 1 2 2v3" />
    </svg>
  );
}

function PlaylistIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <path d="M3 6h13M3 12h13M3 18h7" />
      {/* Filled rather than stroked, for the same reason as the skip icons: a
          triangle this small stroked at the icon's weight closes up. */}
      <polygon className="playlist-cue" points="15.5 13.4 22 17 15.5 20.6" />
    </svg>
  );
}

function BackArrowIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="back-arrow">
      <path d="M10.5 5 3.5 12l7 7" />
      <path d="M3.5 12h17" />
    </svg>
  );
}

function CopyIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" className="control-icon">
      <rect x="8.5" y="8.5" width="9" height="11" rx="1.5" />
      <path d="M15 8.5V7a2 2 0 0 0-2-2H7a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h2" />
    </svg>
  );
}

// How long the pointer can rest on the video while it is playing before the
// cursor hides. Moving the pointer brings it back.
const CURSOR_IDLE_DELAY = 2_000;

// How near the right-hand edge of the screen the pointer has to come before the
// playlist slides in over the picture, and how long the drawer waits after the
// pointer has left it before sliding back out.
const PLAYLIST_EDGE_MARGIN = 24;
const PLAYLIST_HIDE_DELAY = 800;

// What the playlist is doing while the player is fullscreen. It starts hidden;
// the right-hand edge of the screen peeks it out for as long as the pointer
// stays with it, and the P key holds it open until the pointer visits and
// leaves, or until P is pressed again.
type FullscreenPlaylist = "hidden" | "peek" | "held";
type FullscreenMode = "video" | "information" | "controls";

const SPEEDS = [0.1, 0.25, 0.5, 0.75, 1, 1.25, 1.5, 2, 3, 4];
const VOLUME_STEP = 5;

// The whole digit row once stepped the volume, which was a lot of reaching for
// a scale the arrow keys walk in one place. Three of those digits are worth
// keeping, because a preset is pressed once rather than held: each is named by
// the digit its percentage starts with, so 0 is silence, 5 is half and 1 is
// full.
// A preset without a label is drawn as the crossed-out speaker instead, which
// says "silent" without spending the width a written "0%" would.
const VOLUME_PRESETS: {
  key: string;
  volume: number;
  name: string;
  label?: string;
}[] = [
  { key: "0", volume: 0, name: "Mute" },
  { key: "5", volume: 50, name: "Half volume", label: "50%" },
  { key: "1", volume: 100, name: "Full volume", label: "100%" },
];

// How far each skip moves. Ten seconds suits scrubbing past an ad break and
// nothing else: finding the frame someone walks into shot wants a frame, and
// walking through an hour of footage wants minutes. So the step is a choice,
// and both skips take whichever one is chosen.
// A step without `seconds` is one frame, which is only known in seconds once
// the backend says how fast the frames run.
const SKIP_STEPS: { label: string; name: string; seconds?: number }[] = [
  { label: "1f", name: "1 frame" },
  { label: "1s", name: "1 second", seconds: 1 },
  { label: "5s", name: "5 seconds", seconds: 5 },
  { label: "10s", name: "10 seconds", seconds: 10 },
  { label: "30s", name: "30 seconds", seconds: 30 },
  { label: "1m", name: "1 minute", seconds: 60 },
  { label: "5m", name: "5 minutes", seconds: 300 },
  { label: "10m", name: "10 minutes", seconds: 600 },
];

// Where the cycle starts before anything is known about the file: what both
// skips did before they could be changed.
const DEFAULT_SKIP_STEP = SKIP_STEPS.findIndex((step) => step.seconds === 10);

// How far a skip should reach depends on what it is moving through: ten seconds
// is a nudge in a feature and a third of a short clip. A tenth of the file is
// the yardstick — ten of them cross it — so each video opens on whichever step
// lands nearest that, and a tie takes the shorter one, since overshooting the
// moment being looked for is the more tedious mistake to undo.
// The frame step is not a candidate: it measures a rate, not a length.
const skipStepForDuration = (duration: number) => {
  if (!Number.isFinite(duration) || duration <= 0) return undefined;
  const target = duration / 10;
  let best: number | undefined;
  let closest = Number.POSITIVE_INFINITY;
  SKIP_STEPS.forEach((step, index) => {
    if (step.seconds === undefined) return;
    const distance = Math.abs(step.seconds - target);
    if (distance < closest) {
      closest = distance;
      best = index;
    }
  });
  return best;
};

// mpv reads a real frame rate out of the file, but a `<video>` element exposes
// none at all, and neither knows one before the file is open. So a frame is a
// thirtieth of a second until a backend says otherwise — an assumption the
// control names, so a guess is never presented as a measurement.
const ASSUMED_FRAME_RATE = 30;

// The shapes worth cycling between, starting from the one the file itself
// declares. Each is written the way it is spoken and both the number mpv wants
// and the value CSS wants are derived from that, so the two engines cannot be
// given subtly different shapes.
// Ordered from the widest common shape to the squarest and on into the tall
// ones a phone records in, then out to the wide ones a film is cut for, so a
// press moves the picture one step in a direction rather than jumping about.
const ASPECT_RATIOS = [
  "auto",
  "16:9",
  "4:3",
  "1:1",
  "3:4",
  "9:16",
  "21:9",
  "2.39:1",
];

function aspectParts(label: string): [number, number] | undefined {
  const [width, height] = label.split(":").map(Number);
  return width > 0 && height > 0 ? [width, height] : undefined;
}

// What a query is matched against, in the order the controls are read. Each
// part is its own haystack, so turning one off really does stop it being
// searched: a video tagged "home" stops answering to "home" once the tags are
// off, even though the tag is written into its name.
// Bare letters belong to the search field on this screen, so these take Ctrl,
// beside the Ctrl+O that hands a search to another player.
const SEARCH_SCOPES: {
  field: keyof SearchFields;
  key: string;
  name: string;
}[] = [
  { field: "tags", key: "T", name: "tags" },
  { field: "fileName", key: "F", name: "filename" },
  { field: "path", key: "P", name: "path" },
];

// Not Ctrl+F, which a browser reads as "find in page" and which Toka has
// already given to the filename scope above; Ctrl+K is what the rest of the
// world binds "jump to search" to.
const SEARCH_SHORTCUT = "Ctrl+K";

export const THUMBNAIL_SIZE_DEFAULT = 180;
// Keep three additional 20px steps below the original minimum so large result
// sets can be scanned quickly without making the default thumbnails smaller.
export const THUMBNAIL_SIZE_MIN = 60;
export const THUMBNAIL_SIZE_MAX = 360;
export const THUMBNAIL_SIZE_STEP = 20;

// Every result page is a playlist now, so looping has VLC's three states rather
// than a single on/off. Three states cannot be expressed with `aria-pressed`,
// so the control names the one it is in and the pointer or the L key cycles.
type LoopMode = "playlist" | "one" | "off";

const LOOP_MODES: LoopMode[] = ["playlist", "one", "off"];

const LOOP_LABELS: Record<LoopMode, string> = {
  playlist: "Loop: playlist",
  one: "Loop: this video",
  off: "Loop: off",
};

// DOM key names are what `aria-keyshortcuts` wants; these are what a viewer
// should read on the button itself.
const KEY_GLYPHS: Record<string, string> = {
  Escape: "Esc",
  Space: "Space",
  ArrowLeft: "←",
  ArrowRight: "→",
  ArrowUp: "↑",
  ArrowDown: "↓",
  PageUp: "PgUp",
  PageDown: "PgDn",
};

// Whether a keystroke belongs to the control under it rather than to the app.
// The player's bar is full of form controls — the scrubber, the volume slider,
// the speed and subtitle pickers — and none of them take typed text, so
// treating every one of them as an editor left every shortcut dead for as long
// as one of them held focus. A read-only field (like the selectable path)
// is for selecting, not typing — shortcuts remain active there.
function acceptsTypedText(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false;
  const control = target.closest(
    "input, textarea, [contenteditable], [role=textbox]",
  );
  if (!control) return false;
  if (control instanceof HTMLInputElement) {
    if (control.readOnly) return false;
    return ![
      "range",
      "checkbox",
      "radio",
      "button",
      "submit",
      "reset",
    ].includes(control.type);
  }
  if (control instanceof HTMLTextAreaElement && control.readOnly) return false;
  if (
    control.hasAttribute("contenteditable") &&
    control.getAttribute("contenteditable") === "false"
  )
    return false;
  return true;
}

function KeyHint({ shortcut }: { shortcut: string }) {
  const label = shortcut
    .split(" ")
    .map((combination) =>
      combination
        .split("+")
        .map((key) => KEY_GLYPHS[key] ?? key)
        // "ShiftDelete" read as one key nobody has. Every key in a combination
        // is named, and the "+" says they are pressed together.
        .join("+"),
    )
    .join("/");
  // Assistive technology already gets this from aria-keyshortcuts.
  return (
    <span className="key-hint control-label" aria-hidden="true">
      {label}
    </span>
  );
}

// A label centred beside an icon has to be a box of its own: centring an
// anonymous run of text centres its line box, and the descender space in that
// line box pushes the ink a few pixels above the icon it sits next to.
function Label({ children }: { children: string }) {
  return <span className="control-label">{children}</span>;
}

export function shuffleVideos(videos: VideoResult[]): VideoResult[] {
  const shuffled = [...videos];
  for (let index = shuffled.length - 1; index > 0; index -= 1) {
    const swapIndex = Math.floor(Math.random() * (index + 1));
    [shuffled[index], shuffled[swapIndex]] = [
      shuffled[swapIndex],
      shuffled[index],
    ];
  }
  return shuffled;
}

// What tagging a whole page of results did, in a sentence. A page half tagged
// is the interesting case: it says how many were left, and why the first of
// them was, rather than reporting a flat success over a partial rename.
export function taggingOutcome(update: BulkTagUpdate, entry: string): string {
  const count = update.tagged.length;
  const videos = `${count} ${count === 1 ? "video" : "videos"}`;
  const done = `Tagged ${videos} with ${entry}.`;
  if (!update.failed) return done;
  const problem = update.problem ? ` ${update.problem}` : "";
  return `${done} ${update.failed} could not be tagged.${problem}`;
}

// Pairs the declared shortcut with the one shown on the control, so the two
// cannot drift apart as bindings change.
function ControlButton({
  shortcut,
  className = "transport-button",
  children,
  ...rest
}: { shortcut: string } & ButtonHTMLAttributes<HTMLButtonElement>) {
  const { title, ...buttonProps } = rest;
  const tooltip =
    title ??
    (typeof buttonProps["aria-label"] === "string"
      ? buttonProps["aria-label"]
      : undefined);
  return (
    <button
      type="button"
      className={className}
      aria-keyshortcuts={shortcut}
      title={tooltip}
      {...buttonProps}
    >
      {children}
      <KeyHint shortcut={shortcut} />
    </button>
  );
}

function formatTime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  const wholeSeconds = Math.floor(seconds);
  return `${Math.floor(wholeSeconds / 60)}:${String(wholeSeconds % 60).padStart(2, "0")}`;
}

export function playbackSource(filePath: string): string {
  // Linux WebKitGTK does not load media from Tauri's custom asset protocol or
  // file URLs in WebDriver. The E2E fixture server provides an HTTP media URL
  // without changing production or macOS behavior.
  if (
    import.meta.env.VITE_E2E === "1" &&
    navigator.userAgent.includes("Linux")
  ) {
    const fileName = filePath.split(/[\\/]/).at(-1) ?? "";
    const fixturePort = import.meta.env.VITE_E2E_FIXTURE_SERVER_PORT ?? "1421";
    return `http://127.0.0.1:${fixturePort}/${encodeURIComponent(fileName)}`;
  }
  return convertFileSrc(filePath);
}

// Which engines are better asked for a fullscreen window than for a fullscreen
// element. WebKitGTK takes keys for itself once an element is fullscreen — `f`
// leaves fullscreen there, before the page is offered the keystroke — so a tag
// with an f in it could not be typed while the player was fullscreen. The
// window is fullscreen either way as far as the viewer can tell, and this way
// every key still belongs to the app.
function prefersWindowFullscreen(): boolean {
  const agent = navigator.userAgent;
  return agent.includes("Mac OS X") || agent.includes("Linux");
}

function Player({
  videos,
  startIndex,
  hasMore = false,
  onLoadMore,
  onBack,
  onTagsChange,
}: {
  videos: VideoResult[];
  startIndex: number;
  hasMore?: boolean;
  onLoadMore?: () => Promise<VideoResult[]>;
  onBack: () => void;
  onTagsChange: (
    videoId: string,
    update: Pick<VideoResult, "fileName" | "tags">,
  ) => void;
}) {
  const element = useRef<HTMLVideoElement>(null);
  const playerShell = useRef<HTMLDivElement>(null);
  const playerControls = useRef<HTMLDivElement>(null);
  const pointerOverControls = useRef(false);
  const pointerOverPlaylist = useRef(false);
  const nativeSurface = useRef<HTMLDivElement>(null);
  const playlistDrawer = useRef<HTMLElement>(null);
  const fullscreenInfo = useRef<HTMLDivElement>(null);
  const sidecarTracks = useRef<TextTrack[]>([]);
  const [index, setIndex] = useState(startIndex);
  const [prepared, setPrepared] = useState<PreparedVideo>();
  const [duration, setDuration] = useState(0);
  const [currentTime, setCurrentTime] = useState(0);
  const [error, setError] = useState<string>();
  const [nativeStarted, setNativeStarted] = useState(false);
  const [fullscreenError, setFullscreenError] = useState<string>();
  const [fullscreen, setFullscreen] = useState(false);
  const [fullscreenMode, setFullscreenMode] = useState<FullscreenMode>("video");
  const fullscreenModeRef = useRef<FullscreenMode>("video");
  const [showFullscreenInfo, setShowFullscreenInfo] = useState(true);
  // F selects video only, information with the scrubber, then complete controls.
  // Pointer activity cannot rewrite that selection.
  const controlsIdle = fullscreen && fullscreenMode !== "controls";
  const [loop, setLoop] = useState<LoopMode>("playlist");
  const [speed, setSpeed] = useState(1);
  const [volume, setVolume] = useState(100);
  const [playingBack, setPlayingBack] = useState(false);
  const [rotation, setRotation] = useState(0);
  const [aspectStep, setAspectStep] = useState(0);
  const [skipStepIndex, setSkipStepIndex] = useState(DEFAULT_SKIP_STEP);
  // Whatever the backend has been able to say about the open file, so an
  // undefined rate means "nobody knows yet" rather than "no frames".
  const [frameRate, setFrameRate] = useState<number>();
  const [nativeBaseRotation, setNativeBaseRotation] = useState(0);
  const [playlistOpen, setPlaylistOpen] = useState(true);
  const [fullscreenPlaylist, setFullscreenPlaylist] =
    useState<FullscreenPlaylist>("hidden");
  const [nativeSubtitles, setNativeSubtitles] = useState<SubtitleOption[]>([]);
  const [embeddedSubtitles, setEmbeddedSubtitles] = useState<SubtitleOption[]>(
    [],
  );
  const [subtitleIndex, setSubtitleIndex] = useState(-1);
  const [sidecarTextTrack, setSidecarTextTrack] = useState<TextTrack>();
  const [playlist, setPlaylist] = useState(videos);
  const [loadingMore, setLoadingMore] = useState(false);
  const [playlistExhausted, setPlaylistExhausted] = useState(!hasMore);
  const canLoadMore = Boolean(onLoadMore && hasMore && !playlistExhausted);
  const [deletedVideo, setDeletedVideo] = useState<{
    video: VideoResult;
    index: number;
  }>();
  const [addingTag, setAddingTag] = useState(false);
  const [copyFeedback, setCopyFeedback] = useState(false);
  const copyTimeout = useRef<number | undefined>(undefined);
  const [cursorHidden, setCursorHidden] = useState(false);
  const pointerOverVideo = useRef(false);
  const cursorTimer = useRef<number | undefined>(undefined);
  const imageElapsed = useRef(0);
  const video = playlist[index];
  const isImage = isImageResult(video);
  const [slideshowPlaying, setSlideshowPlaying] = useState(true);
  useEffect(() => {
    if (isImage) setSlideshowPlaying(true);
  }, [video.id, isImage]);
  const copyPath = async () => {
    const text = prepared?.filePath ?? video.fileName;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const field = document.createElement("textarea");
        field.value = text;
        field.setAttribute("readonly", "");
        field.style.position = "absolute";
        field.style.left = "-9999px";
        document.body.appendChild(field);
        field.select();
        document.execCommand("copy");
        document.body.removeChild(field);
      }
    } catch {
      // Copy failed silently: the viewer can still select the field manually.
      return;
    }
    setCopyFeedback(true);
    if (copyTimeout.current !== undefined)
      window.clearTimeout(copyTimeout.current);
    copyTimeout.current = window.setTimeout(() => setCopyFeedback(false), 1500);
    focusPlayerShell();
  };
  const copyPathRef = useRef(copyPath);
  useEffect(() => {
    copyPathRef.current = copyPath;
  });
  const updateVideoTags = (update: Pick<VideoResult, "fileName" | "tags">) => {
    setPlaylist((current) =>
      current.map((item) =>
        item.id === video.id ? { ...item, ...update } : item,
      ),
    );
    onTagsChange(video.id, update);
  };
  useEffect(() => {
    if (!deletedVideo) return;
    const timeout = window.setTimeout(() => setDeletedVideo(undefined), 5000);
    return () => window.clearTimeout(timeout);
  }, [deletedVideo]);
  // Speed is a choice about the sitting rather than about one file, so it
  // outlives each video. Read through a ref so loading the next one does not
  // have to depend on it and restart playback whenever it changes.
  const chosenSpeed = useRef(speed);
  const chosenVolume = useRef(volume);
  // Whether the step showing is one the viewer picked for this file or one read
  // from its length, which decides whether the length may still change it.
  const chosenSkipStep = useRef(false);
  useEffect(() => {
    chosenSpeed.current = speed;
  }, [speed]);
  useEffect(() => {
    chosenVolume.current = volume;
  }, [volume]);

  useEffect(() => {
    let active = true;
    let nativeActive = false;
    setPrepared(undefined);
    setDuration(0);
    setCurrentTime(0);
    imageElapsed.current = 0;
    setError(undefined);
    setNativeStarted(false);
    setFullscreenError(undefined);
    setPlayingBack(false);
    setRotation(0);
    setAspectStep(0);
    // Both the step and the rate it may be measured against belong to this
    // file: the next one is read from its own length once that is known.
    setSkipStepIndex(DEFAULT_SKIP_STEP);
    chosenSkipStep.current = false;
    setFrameRate(undefined);
    setNativeBaseRotation(0);
    setNativeSubtitles([]);
    setEmbeddedSubtitles([]);
    setSubtitleIndex(-1);
    setSidecarTextTrack(undefined);
    sidecarTracks.current = [];
    prepareVideo(video.id)
      .then(async (result) => {
        if (!active) return;
        if (!isImage && result.playbackBackend === "native") {
          nativeActive = true;
          setNativeStarted(true);
          await loadNativeVideo(result.filePath);
          const baseRotation = await nativeVideoRotation();
          if (active) setNativeBaseRotation(baseRotation);
          // mpv keeps `video-aspect-override` across files, unlike the speed
          // it does reset, so a shape chosen for the last video would carry
          // silently into this one while the control read "auto".
          await setNativeVideoAspect(-1);
          // mpv starts every file at 1x.
          if (chosenSpeed.current !== 1)
            await setNativeSpeed(chosenSpeed.current);
          if (chosenVolume.current !== 100)
            await setNativeVolume(chosenVolume.current);
          await setNativePaused(false);
          if (active) setPlayingBack(true);
        }
        if (active) {
          setPrepared(result);
          if (isImage) {
            imageElapsed.current = 0;
            setCurrentTime(0);
            setDuration(IMAGE_DURATION_SECONDS);
            setPlayingBack(true);
          }
        }
      })
      .catch((reason: unknown) => {
        if (active) setError(errorMessage(reason));
      });
    return () => {
      active = false;
      if (nativeActive) {
        void setNativeVideoBounds({
          x: 0,
          y: 0,
          width: 1,
          height: 1,
          visible: false,
        }).catch(() => {});
        void stopNativeVideo().catch(() => {});
      }
    };
  }, [video.id]);

  const native = !isImage && prepared?.playbackBackend === "native";
  // GTK composites the native surface above the WebView. An invalid file can
  // fail after that surface has been started, so an error view alone is not
  // enough: the native layer must be hidden before it can cover the view (or
  // the search results shown after Back) at its previous cropped bounds.
  useEffect(() => {
    if (!error || !nativeStarted) return;
    void setNativeVideoBounds({
      x: 0,
      y: 0,
      width: 1,
      height: 1,
      visible: false,
    }).catch(() => {});
    void stopNativeVideo().catch(() => {});
  }, [error, nativeStarted]);
  // Windowed playback keeps the drawer permanently; fullscreen is for watching,
  // so there it starts hidden and is summoned instead.
  const drawerOpen = fullscreen
    ? fullscreenPlaylist !== "hidden"
    : playlistOpen;

  const togglePlaylist = () => {
    if (fullscreen)
      setFullscreenPlaylist((state) =>
        state === "hidden" ? "held" : "hidden",
      );
    else setPlaylistOpen((open) => !open);
  };

  // How far through the video the scrubber is, as the sliver it collapses to in
  // fullscreen has to paint its own fill.
  const elapsedShare =
    duration > 0
      ? Math.min(100, Math.max(0, (currentTime / duration) * 100))
      : 0;

  const subtitles = useMemo<SubtitleOption[]>(() => {
    if (native) return nativeSubtitles;
    const sidecars = (prepared?.subtitles ?? [])
      .filter((subtitle) => subtitle.webPlayable)
      .map<SubtitleOption>((subtitle) => ({
        source: "sidecar",
        label: subtitle.label,
        language: subtitle.language,
        track: subtitle.track,
      }));
    return [...sidecars, ...embeddedSubtitles];
  }, [embeddedSubtitles, native, nativeSubtitles, prepared]);

  const selectedSubtitle = subtitles[subtitleIndex];

  useEffect(() => {
    // The error view replaces the surface these measurements come from, so the
    // watchers have to go with it: left running, the next resize would measure
    // a view that no longer has a picture in it and put the hidden layer back
    // over whatever is on screen, and the poll would keep asking an engine that
    // has already been stopped how far along it is.
    if (!native || error || !nativeSurface.current) return;
    const surface = nativeSurface.current;
    let advancing = false;
    // mpv only knows the file's subtitle tracks once it has finished loading,
    // so the playback poll also asks for them — until it finds some, or until
    // the file has clearly had long enough to report that it has none.
    let subtitleLookupsLeft = 20;
    const updateBounds = () => {
      const bounds = surface.getBoundingClientRect();
      const controls = playerControls.current?.getBoundingClientRect();
      const drawer = playlistDrawer.current?.getBoundingClientRect();
      const info = fullscreenInfo.current?.getBoundingClientRect();
      // GTK renders the native GL area above the opaque WebView. Trim its
      // bounds around WebView controls and the playlist so the real decoded
      // picture remains visible without covering those interactive elements.
      // The fullscreen overlay is trimmed around for the same reason: it is
      // read over the picture everywhere else, but on Linux the picture would
      // simply be painted on top of it.
      // The path and the clock share one row along the bottom, so the overlay
      // costs the picture a single strip instead of one at either end — and
      // while the controls are up that strip is inside the one they already
      // reserve, so showing the overlay then costs nothing at all.
      const floors = [bounds.bottom];
      if (controls && !controlsIdle) floors.push(controls.top);
      if (info) floors.push(info.top);
      const visibleHeight = Math.max(1, Math.min(...floors) - bounds.top);
      const visibleWidth = drawer
        ? Math.max(1, Math.min(bounds.width, drawer.left - bounds.left))
        : bounds.width;
      void setNativeVideoBounds({
        x: Math.round(bounds.x),
        y: Math.round(bounds.top),
        width: Math.round(visibleWidth),
        height: Math.round(visibleHeight),
        visible: visibleWidth > 0 && visibleHeight > 0,
      }).catch((reason: unknown) => setError(errorMessage(reason)));
    };
    updateBounds();
    const observer = new ResizeObserver(updateBounds);
    observer.observe(surface);
    window.addEventListener("resize", updateBounds);
    const poll = window.setInterval(() => {
      void nativePlaybackState()
        .then((state) => {
          setDuration(state.duration);
          setCurrentTime(state.currentTime);
          // mpv only knows the rate once it has read the file, so this arrives
          // with the polling rather than with the load.
          if (state.frameRate) setFrameRate(state.frameRate);
          if (!state.ended) advancing = false;
          if (state.ended && !advancing) {
            advancing = true;
            finishVideo.current(state.duration);
          }
        })
        .catch((reason: unknown) => setError(errorMessage(reason)));
      if (subtitleLookupsLeft > 0) {
        subtitleLookupsLeft -= 1;
        void nativeSubtitleTracks()
          .then((tracks) => {
            if (tracks.length === 0) return;
            subtitleLookupsLeft = 0;
            setNativeSubtitles(
              tracks.map((track) => ({
                source: "native",
                label: track.label,
                id: track.id,
              })),
            );
          })
          .catch(() => {});
      }
    }, 250);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", updateBounds);
      window.clearInterval(poll);
    };
  }, [
    controlsIdle,
    drawerOpen,
    error,
    fullscreen,
    index,
    native,
    showFullscreenInfo,
  ]);

  const play = () => {
    if (native) {
      void setNativePaused(false)
        .then(() => setPlayingBack(true))
        .catch((reason: unknown) => setError(errorMessage(reason)));
      return;
    }
    void element.current
      ?.play()
      .then(() => setPlayingBack(true))
      .catch(() =>
        setError("This video could not be played by the system media engine."),
      );
  };

  const pause = () => {
    if (native) {
      void setNativePaused(true)
        .then(() => setPlayingBack(false))
        .catch((reason: unknown) => setError(errorMessage(reason)));
    } else {
      element.current?.pause();
      setPlayingBack(false);
    }
  };

  // Typing a tag needs the viewer's attention, so the picture waits for them.
  const openTagField = () => {
    if (playingBack) pause();
    setAddingTag(true);
  };

  const restart = () => {
    if (native) {
      void seekNativeVideo(0)
        .then(() => setNativePaused(false))
        .then(() => setPlayingBack(true))
        .catch((reason: unknown) => setError(errorMessage(reason)));
    } else if (element.current) {
      element.current.currentTime = 0;
      play();
    }
    setCurrentTime(0);
  };

  // Search playlists start with the results already on screen. Fetching the
  // next page only when playback reaches it keeps a large search from opening
  // hundreds of backend requests and thousands of drawer rows at once.
  const loadMoreAndAdvance = useCallback(async () => {
    if (!canLoadMore || !onLoadMore || loadingMore) return;
    setLoadingMore(true);
    try {
      const nextVideos = await onLoadMore();
      if (!nextVideos.length) {
        setPlaylistExhausted(true);
        return;
      }
      setPlaylist((current) => [...current, ...nextVideos]);
      setIndex((current) => current + 1);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setLoadingMore(false);
    }
  }, [canLoadMore, loadingMore, onLoadMore]);

  // What happens when the current video runs out. Both backends end a video the
  // same way, but the native one only finds out by polling, so the poll reaches
  // this through a ref rather than rebuilding its interval whenever the loop
  // mode or the position in the playlist changes.
  const endOfVideo = (endTime: number) => {
    // Neither engine reports a final time that quite reaches the duration, so
    // the scrubber stopped five to ten percent short and made every video look
    // as though its ending had been skipped.
    if (endTime > 0) setCurrentTime(endTime);
    if (loop === "one") {
      restart();
      return;
    }
    // "Off" means play the video the viewer is on and stop there, so it does
    // not move on to the next entry even in the middle of a playlist.
    if (loop === "off") return;
    const next = index < playlist.length - 1 ? index + 1 : 0;
    if (index === playlist.length - 1 && canLoadMore) {
      void loadMoreAndAdvance();
      return;
    }
    // Wrapping a one-entry playlist lands back on the video that just ended,
    // and React drops a state change that changes nothing, so that entry has to
    // be started again rather than waited on.
    if (next === index) restart();
    else setIndex(next);
  };
  const finishVideo = useRef(endOfVideo);
  useEffect(() => {
    finishVideo.current = endOfVideo;
  });

  const cycleLoop = () =>
    setLoop(
      (mode) => LOOP_MODES[(LOOP_MODES.indexOf(mode) + 1) % LOOP_MODES.length],
    );

  const rotate = (amount: number) => {
    setRotation((current) => {
      const next = (current + amount + 360) % 360;
      if (native) {
        const degrees = (nativeBaseRotation + next) % 360;
        void setNativeVideoRotation(degrees).catch((reason: unknown) =>
          setError(errorMessage(reason)),
        );
      }
      return next;
    });
  };

  const aspect = ASPECT_RATIOS[aspectStep];
  const aspectSides = aspectParts(aspect);

  // Deliberately independent of rotation. Some players apply an override
  // sideways once a video is turned, which makes the control unguessable; here
  // a CSS aspect-ratio is a layout property while `rotate()` is a post-layout
  // transform, and mpv applies `video-aspect-override` to the source before
  // `video-rotate`. Both paths therefore act as if the video were upright.
  const cycleAspect = () => {
    setAspectStep((current) => {
      const next = (current + 1) % ASPECT_RATIOS.length;
      const sides = aspectParts(ASPECT_RATIOS[next]);
      if (native)
        // A negative ratio is what hands the shape back to mpv, rather than
        // pinning it to whatever was chosen last.
        void setNativeVideoAspect(sides ? sides[0] / sides[1] : -1).catch(
          (reason: unknown) => setError(errorMessage(reason)),
        );
      return next;
    });
  };

  // The length lands after the file opens — and on the native path a poll
  // repeats it — so the default waits for it, and stands aside for good once
  // the viewer has said what they want for this file.
  useEffect(() => {
    if (isImage || chosenSkipStep.current) return;
    const step = skipStepForDuration(duration);
    if (step !== undefined) setSkipStepIndex(step);
  }, [duration, isImage]);

  const skipStep = SKIP_STEPS[skipStepIndex];
  // A frame is only as exact as the rate behind it, so the control says which
  // rate that is and whether anybody measured it.
  const skipSeconds = skipStep.seconds ?? 1 / (frameRate ?? ASSUMED_FRAME_RATE);
  const measuredRate = frameRate
    ? `${Number(frameRate.toFixed(3))} fps`
    : `assumed ${ASSUMED_FRAME_RATE} fps`;
  const skipStepName = skipStep.seconds
    ? skipStep.name
    : `${skipStep.name} (${measuredRate})`;

  const cycleSkipStep = () => {
    chosenSkipStep.current = true;
    setSkipStepIndex((current) => (current + 1) % SKIP_STEPS.length);
  };

  const skip = (amount: number) => {
    const next = Math.max(
      0,
      Math.min(duration || Number.POSITIVE_INFINITY, currentTime + amount),
    );
    if (native)
      void seekNativeVideo(next).catch((reason: unknown) =>
        setError(errorMessage(reason)),
      );
    else if (element.current) element.current.currentTime = next;
    setCurrentTime(next);
  };

  const selectVideo = (nextIndex: number) => {
    if (nextIndex >= 0 && nextIndex < playlist.length) setIndex(nextIndex);
  };

  const shufflePlaylist = () => {
    setPlaylist((current) => shuffleVideos(current));
    setIndex(0);
  };

  const moveVideo = (direction: -1 | 1) => {
    if (playlist.length === 0) return;
    if (direction === 1 && index === playlist.length - 1 && canLoadMore) {
      void loadMoreAndAdvance();
      return;
    }
    setIndex(
      (current) => (current + direction + playlist.length) % playlist.length,
    );
  };

  const toggleSlideshow = () => setSlideshowPlaying((playing) => !playing);

  useEffect(() => {
    if (!isImage || !slideshowPlaying) return;
    const timer = window.setInterval(() => {
      imageElapsed.current = Math.min(
        IMAGE_DURATION_SECONDS,
        Math.round(
          (imageElapsed.current + IMAGE_PROGRESS_TICK / 1_000) * 1_000,
        ) / 1_000,
      );
      setCurrentTime(imageElapsed.current);
      if (imageElapsed.current < IMAGE_DURATION_SECONDS) return;
      if (loop === "off" && index === playlist.length - 1) return;
      if (playlist.length <= 1 && loop === "one") {
        imageElapsed.current = 0;
        setCurrentTime(0);
        return;
      }
      imageElapsed.current = 0;
      setCurrentTime(0);
      if (index === playlist.length - 1 && canLoadMore && !loadingMore) {
        void loadMoreAndAdvance();
        return;
      }
      setIndex((current) => {
        if (loop === "off" && current === playlist.length - 1) return current;
        return (current + 1) % playlist.length;
      });
    }, IMAGE_PROGRESS_TICK);
    return () => window.clearInterval(timer);
  }, [
    canLoadMore,
    index,
    isImage,
    loadMoreAndAdvance,
    loadingMore,
    loop,
    playlist.length,
    slideshowPlaying,
  ]);

  const removeCurrentVideo = async () => {
    const removed = playlist[index];
    try {
      await deleteVideo(removed.id);
      setDeletedVideo({ video: removed, index });
      if (playlist.length === 1) {
        onBack();
        return;
      }
      setPlaylist((current) =>
        current.filter((item) => item.id !== removed.id),
      );
      setIndex((current) =>
        Math.min(current, Math.max(0, playlist.length - 2)),
      );
    } catch (reason) {
      setError(errorMessage(reason));
    }
  };

  const restoreDeletedVideo = async () => {
    if (!deletedVideo) return;
    try {
      await undoDelete();
      setPlaylist((current) => [
        ...current.slice(0, deletedVideo.index),
        deletedVideo.video,
        ...current.slice(deletedVideo.index),
      ]);
      setIndex(deletedVideo.index);
      setDeletedVideo(undefined);
    } catch (reason) {
      setError(errorMessage(reason));
    }
  };

  const applySpeed = (next: number) => {
    setSpeed(next);
    if (native)
      void setNativeSpeed(next).catch((reason: unknown) =>
        setError(errorMessage(reason)),
      );
    else if (element.current) element.current.playbackRate = next;
  };

  const applyVolume = (next: number) => {
    const clamped = Math.max(0, Math.min(100, next));
    setVolume(clamped);
    if (native)
      void setNativeVolume(clamped).catch((reason: unknown) =>
        setError(errorMessage(reason)),
      );
    else if (element.current) element.current.volume = clamped / 100;
  };

  // Holds at the ends of the range rather than wrapping, so holding the key
  // down cannot jump from slowest straight back to fastest.
  const stepSpeed = (direction: number) => {
    const at = SPEEDS.indexOf(speed);
    const next =
      SPEEDS[
        Math.min(
          SPEEDS.length - 1,
          Math.max(0, (at < 0 ? SPEEDS.indexOf(1) : at) + direction),
        )
      ];
    if (next !== speed) applySpeed(next);
  };

  const selectSubtitle = (nextIndex: number) => {
    const option = subtitles[nextIndex];
    setSubtitleIndex(option ? nextIndex : -1);
    if (native) {
      void setNativeSubtitle(
        option?.source === "native" ? option.id : null,
      ).catch((reason: unknown) => setError(errorMessage(reason)));
      return;
    }
    if (option?.source !== "sidecar") {
      setSidecarTextTrack(undefined);
      return;
    }
    setSidecarTextTrack(undefined);
    void subtitleCues(video.id, option.track)
      .then((cues) => {
        const media = element.current;
        if (!media) return;
        const textTrack = media.addTextTrack(
          "subtitles",
          option.label,
          option.language ?? "",
        );
        sidecarTracks.current.push(textTrack);
        for (const cue of parseWebVttCues(cues)) {
          textTrack.addCue(new VTTCue(cue.startTime, cue.endTime, cue.text));
        }
        setSidecarTextTrack(textTrack);
      })
      .catch((reason: unknown) => setError(errorMessage(reason)));
  };

  const toggleSubtitles = () => selectSubtitle(subtitleIndex >= 0 ? -1 : 0);

  // The web engine surfaces tracks carried inside the file itself; they join
  // the list beside the sidecar files Rust found.
  useEffect(() => {
    const media = element.current;
    if (native || !media?.textTracks) return;
    const sync = () => {
      const own = media.querySelector("track")?.track;
      const found: SubtitleOption[] = [];
      for (
        let position = 0;
        position < media.textTracks.length;
        position += 1
      ) {
        const textTrack = media.textTracks[position];
        if (
          textTrack === own ||
          sidecarTracks.current.includes(textTrack) ||
          (textTrack.kind !== "subtitles" && textTrack.kind !== "captions")
        ) {
          continue;
        }
        found.push({
          source: "embedded",
          label:
            textTrack.label ||
            textTrack.language.toUpperCase() ||
            `Track ${position + 1}`,
          textTrack,
        });
      }
      setEmbeddedSubtitles((current) =>
        current.length === found.length &&
        current.every((option, at) => option.label === found[at].label)
          ? current
          : found,
      );
    };
    sync();
    media.textTracks.addEventListener?.("addtrack", sync);
    media.textTracks.addEventListener?.("removetrack", sync);
    return () => {
      media.textTracks.removeEventListener?.("addtrack", sync);
      media.textTracks.removeEventListener?.("removetrack", sync);
    };
  }, [native, prepared]);

  // A text track stays invisible until its mode is "showing".
  useEffect(() => {
    const media = element.current;
    if (native || !media?.textTracks) return;
    const own = media.querySelector("track")?.track;
    for (let position = 0; position < media.textTracks.length; position += 1) {
      const textTrack = media.textTracks[position];
      const showing =
        selectedSubtitle?.source === "sidecar"
          ? textTrack === sidecarTextTrack || textTrack === own
          : selectedSubtitle?.source === "embedded" &&
            selectedSubtitle.textTrack === textTrack;
      textTrack.mode = showing ? "showing" : "disabled";
    }
  }, [native, selectedSubtitle, sidecarTextTrack]);

  useEffect(() => {
    const updateFullscreen = () =>
      setFullscreen(document.fullscreenElement === playerShell.current);
    document.addEventListener("fullscreenchange", updateFullscreen);
    return () =>
      document.removeEventListener("fullscreenchange", updateFullscreen);
  }, []);

  // Fullscreen owns a transient playlist drawer; windowed playback uses the
  // permanent drawer instead.
  useEffect(() => {
    if (fullscreen) return;
    setFullscreenPlaylist("hidden");
    pointerOverPlaylist.current = false;
  }, [fullscreen]);

  // Bringing the pointer all the way to the right-hand edge of the screen slides
  // the playlist out over the picture; it stays for as long as the pointer is on
  // it and goes again shortly after the pointer leaves.
  useEffect(() => {
    if (!fullscreen) return;
    let lastNearPlaylist = 0;
    const follow = (event: MouseEvent) => {
      if (event.clientX < window.innerWidth - PLAYLIST_EDGE_MARGIN) return;
      lastNearPlaylist = Date.now();
      setFullscreenPlaylist((state) => (state === "hidden" ? "peek" : state));
    };
    // Polling lets the pointer rest on the drawer to hold it open without
    // re-arming a one-shot timer on every movement.
    const tick = window.setInterval(() => {
      if (pointerOverPlaylist.current) {
        lastNearPlaylist = Date.now();
        return;
      }
      if (Date.now() - lastNearPlaylist >= PLAYLIST_HIDE_DELAY) {
        setFullscreenPlaylist((state) => (state === "peek" ? "hidden" : state));
      }
    }, 100);
    window.addEventListener("mousemove", follow);
    return () => {
      window.clearInterval(tick);
      window.removeEventListener("mousemove", follow);
    };
  }, [fullscreen]);

  // Hide the cursor while it rests on the picture during playback. It shows
  // again at once on movement and whenever it is not over the picture, so it
  // never gets lost on the controls or the rest of the page.
  useEffect(() => {
    const clearTimer = () => {
      if (cursorTimer.current !== undefined) {
        window.clearTimeout(cursorTimer.current);
        cursorTimer.current = undefined;
      }
    };
    const scheduleHide = () => {
      clearTimer();
      if (
        !playingBack ||
        !pointerOverVideo.current ||
        pointerOverControls.current
      )
        return;
      cursorTimer.current = window.setTimeout(
        () => setCursorHidden(true),
        CURSOR_IDLE_DELAY,
      );
    };
    const isOverVideo = (target: EventTarget | null) => {
      if (!(target instanceof Element)) return false;
      const videoEl = element.current;
      const surfaceEl = nativeSurface.current;
      return (
        target === videoEl ||
        target === surfaceEl ||
        (videoEl?.contains(target) ?? false) ||
        (surfaceEl?.contains(target) ?? false)
      );
    };
    if (!playingBack) {
      clearTimer();
      setCursorHidden(false);
      return;
    }
    scheduleHide();
    const onPointerMove = (event: MouseEvent | PointerEvent) => {
      const over = isOverVideo(event.target);
      pointerOverVideo.current = over;
      // Moving anywhere brings the cursor back; if it is still over the
      // picture a new idle countdown begins.
      setCursorHidden(false);
      if (over) scheduleHide();
      else clearTimer();
    };
    const onLeaveVideo = () => {
      pointerOverVideo.current = false;
      setCursorHidden(false);
      clearTimer();
    };
    const onKeyDown = () => {
      setCursorHidden(false);
      scheduleHide();
    };
    window.addEventListener("mousemove", onPointerMove);
    window.addEventListener("pointermove", onPointerMove);
    window.addEventListener("mouseout", (event) => {
      // Leaving the video element fires mouseout with relatedTarget outside.
      if (
        isOverVideo(event.target) &&
        !isOverVideo(event.relatedTarget as EventTarget)
      ) {
        onLeaveVideo();
      }
    });
    window.addEventListener("keydown", onKeyDown);
    return () => {
      clearTimer();
      window.removeEventListener("mousemove", onPointerMove);
      window.removeEventListener("pointermove", onPointerMove);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [playingBack]);

  useEffect(() => {
    return () => {
      if (cursorTimer.current !== undefined)
        window.clearTimeout(cursorTimer.current);
    };
  }, []);

  const focusPlayerShell = () => {
    playerShell.current?.focus({ preventScroll: true });
  };

  // No control in the bar keeps the keyboard once it has been used: a button
  // that held focus after a click kept a focus ring lit long after it had
  // stopped doing anything, and the player is what the next keystroke is meant
  // for. A dropdown is left alone here because its list needs the keyboard
  // while it is open — it hands focus back from its own change handler — and so
  // is the tag field, which exists to be typed into.
  const releaseFocus = (event: { target: EventTarget }) => {
    if (
      event.target instanceof Element &&
      event.target.closest("select, textarea, [contenteditable]")
    )
      return;
    if (acceptsTypedText(event.target)) return;
    focusPlayerShell();
  };

  const leaveFullscreen = () => {
    setFullscreenError(undefined);
    if (document.fullscreenElement) {
      const exiting = document.exitFullscreen?.();
      if (exiting) {
        void exiting
          .then(focusPlayerShell)
          .catch(() =>
            getCurrentWindow()
              .setFullscreen(false)
              .then(() => {
                setFullscreen(false);
                focusPlayerShell();
              }),
          )
          .catch((reason: unknown) => setFullscreenError(errorMessage(reason)));
      }
      return;
    }
    if (fullscreen) {
      void getCurrentWindow()
        .setFullscreen(false)
        .then(() => {
          setFullscreen(false);
          focusPlayerShell();
        })
        .catch((reason: unknown) => setFullscreenError(errorMessage(reason)));
      return;
    }
  };

  const cycleFullscreen = () => {
    if (fullscreen) {
      if (fullscreenModeRef.current === "video") {
        setShowFullscreenInfo(true);
        fullscreenModeRef.current = "information";
        setFullscreenMode("information");
        return;
      }
      if (fullscreenModeRef.current === "information") {
        fullscreenModeRef.current = "controls";
        setFullscreenMode("controls");
        return;
      }
      leaveFullscreen();
      return;
    }
    fullscreenModeRef.current = "video";
    setFullscreenMode("video");
    setShowFullscreenInfo(false);
    setFullscreenError(undefined);
    const shell = playerShell.current;
    const enterWindowFullscreen = () =>
      getCurrentWindow()
        .setFullscreen(true)
        .then(() => {
          setFullscreen(true);
          focusPlayerShell();
        });
    const enterDocumentFullscreen = () => {
      if (!shell?.requestFullscreen)
        return Promise.reject(
          new Error("Fullscreen mode is not supported by this system."),
        );
      return shell.requestFullscreen();
    };
    const entering = prefersWindowFullscreen()
      ? enterWindowFullscreen().catch(enterDocumentFullscreen)
      : enterDocumentFullscreen().catch(enterWindowFullscreen);
    void entering
      .then(focusPlayerShell)
      .catch((reason: unknown) => setFullscreenError(errorMessage(reason)));
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        event.defaultPrevented ||
        (event.ctrlKey && !(event.shiftKey && event.key === "Delete")) ||
        event.metaKey ||
        event.altKey ||
        acceptsTypedText(target)
      )
        return;

      if (
        target instanceof HTMLInputElement &&
        target.classList.contains("player-timeline") &&
        (event.key === "ArrowLeft" || event.key === "ArrowRight")
      )
        return;

      const run = (action: () => void) => {
        event.preventDefault();
        action();
      };
      const volumePreset = VOLUME_PRESETS.find(
        (preset) => preset.key === event.key,
      );
      // Every keystroke belongs to the open tag field: binding them here would
      // hijack the letters being typed, and Escape has to close the field
      // rather than leave fullscreen. Reached only when the field does not hold
      // focus itself, which the check above already returns on.
      // Every keystroke belongs to the open tag field: binding them here would
      // hijack the letters being typed, and Escape has to close the field
      // rather than leave fullscreen. Reached only when the field does not hold
      // focus itself, which the check above already returns on.
      if (addingTag) {
        if (event.key === "Escape") run(() => setAddingTag(false));
        return;
      }
      if (event.key === " " || event.key === "Spacebar") {
        run(isImage ? toggleSlideshow : playingBack ? pause : play);
      } else if (event.key === "[") {
        run(() => rotate(-90));
      } else if (event.key === "]") {
        run(() => rotate(90));
      } else if (event.key === "ArrowLeft") {
        run(() => (isImage ? moveVideo(-1) : skip(-skipSeconds)));
      } else if (event.key === "ArrowRight") {
        run(() => (isImage ? moveVideo(1) : skip(skipSeconds)));
      } else if (event.key.toLowerCase() === "j") {
        if (!isImage) run(cycleSkipStep);
        else return;
      } else if (event.key === "PageUp") {
        run(() => moveVideo(-1));
      } else if (event.key === "PageDown") {
        run(() => moveVideo(1));
      } else if (event.key === "Delete" && event.shiftKey && event.ctrlKey) {
        run(() => void restoreDeletedVideo());
      } else if (event.key === "Delete" && event.shiftKey) {
        run(() => void removeCurrentVideo());
      } else if (event.key.toLowerCase() === "r") {
        run(shufflePlaylist);
      } else if (event.key === "-") {
        run(() => stepSpeed(-1));
      } else if (event.key === "=" || event.key === "+") {
        run(() => stepSpeed(1));
      } else if (event.key === "ArrowDown") {
        run(() => applyVolume(volume - VOLUME_STEP));
      } else if (event.key === "ArrowUp") {
        run(() => applyVolume(volume + VOLUME_STEP));
      } else if (volumePreset) {
        run(() => applyVolume(volumePreset.volume));
      } else if (event.key.toLowerCase() === "a") {
        run(cycleAspect);
      } else if (event.key.toLowerCase() === "c") {
        run(() => void copyPathRef.current());
      } else if (event.key.toLowerCase() === "t") {
        run(openTagField);
      } else if (event.key.toLowerCase() === "s") {
        if (isImage) run(toggleSlideshow);
        else if (subtitles.length > 0) run(toggleSubtitles);
      } else if (event.key.toLowerCase() === "l") {
        run(cycleLoop);
      } else if (event.key.toLowerCase() === "p") {
        run(togglePlaylist);
      } else if (event.key.toLowerCase() === "f") {
        run(cycleFullscreen);
      } else if (event.key.toLowerCase() === "i" && fullscreen) {
        run(() => setShowFullscreenInfo((visible) => !visible));
      } else if (event.key === "Escape") {
        run(fullscreen ? leaveFullscreen : onBack);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    currentTime,
    duration,
    fullscreen,
    index,
    isImage,
    native,
    nativeBaseRotation,
    onBack,
    addingTag,
    playingBack,
    playlist.length,
    skipSeconds,
    slideshowPlaying,
    speed,
    subtitleIndex,
    subtitles,
    videos.length,
    volume,
  ]);

  if (error) {
    const unsupported = error.includes("format") || error.includes("codec");
    return (
      <section
        className="player-error-state"
        aria-label={`Unable to play ${video.fileName}`}
      >
        <div className="unsupported-icon" aria-hidden="true">
          <span />
        </div>
        <h1>
          {unsupported
            ? "This video format isn't supported on your computer"
            : "This video could not be played"}
        </h1>
        <p>{video.fileName}</p>
        <p role="alert" className="sr-only">
          {error}
        </p>
        <div className="error-actions">
          <ControlButton
            shortcut="Escape"
            className="back-button"
            onClick={onBack}
            aria-label="Back to results"
          >
            <BackArrowIcon />
            <Label>Back to results</Label>
          </ControlButton>
          {index < playlist.length - 1 || canLoadMore ? (
            <ControlButton
              shortcut="PageDown"
              className="playlist-button"
              aria-label="Skip to next video"
              title="Skip to next video"
              disabled={loadingMore}
              onClick={() =>
                index < playlist.length - 1
                  ? setIndex((current) => current + 1)
                  : void loadMoreAndAdvance()
              }
            >
              <Label>Skip to next</Label>
            </ControlButton>
          ) : null}
        </div>
      </section>
    );
  }

  return (
    <section
      className="player-view"
      aria-label={`Player for ${video.fileName}`}
    >
      <div className="player-heading">
        <ControlButton
          shortcut="Escape"
          className="back-button"
          onClick={onBack}
          aria-label="Back to results"
        >
          <BackArrowIcon />
          <Label>Back</Label>
        </ControlButton>
        {/* The name is what a viewer looks for; the folder it came from is
            what tells two identically named files apart. Fullscreen has no
            room for either, which is what the overlay is for. */}
        <div className="player-title">
          <h1 title={prepared?.filePath ?? video.fileName}>{video.fileName}</h1>
          {prepared ? (
            <div className="player-path-row">
              <input
                className="player-file-path"
                title={prepared.filePath}
                value={prepared.filePath}
                readOnly
                onFocus={(event) => event.currentTarget.select()}
                onClick={(event) => event.currentTarget.select()}
                aria-label="File path"
              />
              <ControlButton
                shortcut="C"
                className="copy-button"
                aria-label={copyFeedback ? "Copied" : "Copy file path"}
                onClick={() => void copyPath()}
              >
                {copyFeedback ? (
                  <Label>Copied</Label>
                ) : (
                  <>
                    <CopyIcon />
                    <Label>Copy</Label>
                  </>
                )}
              </ControlButton>
            </div>
          ) : null}
        </div>
        <ControlButton
          shortcut="P"
          className="playlist-toggle"
          aria-expanded={drawerOpen}
          title="Toggle playlist"
          onClick={togglePlaylist}
        >
          <Label>Playlist</Label>
          <span className="playlist-count">
            <Label>{String(playlist.length)}</Label>
          </span>
        </ControlButton>
      </div>

      {fullscreenError ? (
        <p role="alert" className="message error">
          {fullscreenError}
        </p>
      ) : null}
      {/* The shell outlives each video: it is the element the browser promotes
          to fullscreen, and unmounting it between playlist items dropped the
          window back out of fullscreen. */}
      {/* The `fullscreen` class carries the same rules as `:fullscreen`, so the
          layout can be driven and measured without asking an engine that may
          refuse the request to grant it. */}
      <div
        ref={playerShell}
        className={`player-shell${fullscreen ? " fullscreen" : ""}${fullscreen && fullscreenMode === "video" ? " video-only" : ""}${controlsIdle ? " idle" : ""}${cursorHidden ? " cursor-hidden" : ""}`}
        tabIndex={-1}
        onMouseMove={() => {
          if (cursorHidden) setCursorHidden(false);
        }}
      >
        {!prepared ? (
          <p className="message preparing-video">Preparing video…</p>
        ) : isImage ? (
          <img
            src={playbackSource(prepared.filePath)}
            alt={`Playing ${video.fileName}`}
            aria-label={`Playing ${video.fileName}`}
            className="slideshow-image"
            style={{
              transform: `rotate(${rotation}deg)`,
              ...(aspectSides
                ? {
                    aspectRatio: `${aspectSides[0]} / ${aspectSides[1]}`,
                    objectFit: "contain" as const,
                  }
                : {}),
            }}
          />
        ) : native ? (
          <div
            ref={nativeSurface}
            className="native-video"
            aria-label={`Playing ${video.fileName}`}
          />
        ) : (
          <video
            ref={element}
            src={playbackSource(prepared.filePath)}
            aria-label={`Playing ${video.fileName}`}
            style={{
              transform: `rotate(${rotation}deg)`,
              // `fill` matters as much as the ratio: left on `contain` the
              // picture is letterboxed back into its original shape inside the
              // new box, and nothing appears to have changed.
              ...(aspectSides
                ? {
                    aspectRatio: `${aspectSides[0]} / ${aspectSides[1]}`,
                    objectFit: "fill" as const,
                  }
                : {}),
            }}
            onLoadedMetadata={(event) => {
              setDuration(
                Number.isFinite(event.currentTarget.duration)
                  ? event.currentTarget.duration
                  : 0,
              );
              event.currentTarget.playbackRate = speed;
              event.currentTarget.volume = volume / 100;
              play();
            }}
            onPlay={() => setPlayingBack(true)}
            onPause={() => setPlayingBack(false)}
            onTimeUpdate={(event) =>
              setCurrentTime(event.currentTarget.currentTime)
            }
            onEnded={(event) =>
              endOfVideo(duration || event.currentTarget.duration || 0)
            }
            onError={() =>
              setError(
                "This video format or codec is not supported on this computer.",
              )
            }
          ></video>
        )}
        {fullscreen && fullscreenMode !== "video" && showFullscreenInfo ? (
          <div
            ref={fullscreenInfo}
            className="fullscreen-info"
            role="region"
            aria-label="Fullscreen video information"
          >
            <div className="fullscreen-file-path">
              {prepared?.filePath ?? video.fileName}
            </div>
            <div className="fullscreen-time">
              {formatTime(currentTime)} / {formatTime(duration)}
            </div>
          </div>
        ) : null}
        <div
          ref={playerControls}
          className={controlsIdle ? "player-controls idle" : "player-controls"}
          hidden={fullscreen && fullscreenMode === "video"}
          aria-label="Video controls"
          onClick={releaseFocus}
          onMouseEnter={() => {
            pointerOverControls.current = true;
          }}
          onMouseLeave={() => {
            pointerOverControls.current = false;
          }}
        >
          <input
            className="player-timeline"
            aria-label={isImage ? "Image timeline" : "Video timeline"}
            title={isImage ? "Image timeline" : "Video timeline"}
            type="range"
            min="0"
            max={duration || 0}
            step="0.1"
            value={Math.min(currentTime, duration || 0)}
            // The sliver the bar collapses to in fullscreen paints its own fill:
            // a range input's track and thumb cannot be drawn legibly at six
            // pixels, so the progress is handed to the stylesheet instead.
            style={{ "--progress": `${elapsedShare}%` } as CSSProperties}
            onChange={(event) => {
              const nextTime = Number(event.currentTarget.value);
              if (isImage) {
                imageElapsed.current = nextTime;
                setCurrentTime(nextTime);
                return;
              }
              if (native)
                void seekNativeVideo(nextTime).catch((reason: unknown) =>
                  setError(errorMessage(reason)),
                );
              else if (element.current) element.current.currentTime = nextTime;
              setCurrentTime(nextTime);
            }}
          />
          <div className="player-transport">
            <ControlButton
              shortcut="PageUp"
              onClick={() => moveVideo(-1)}
              aria-label="Previous video"
            >
              <PreviousIcon />
            </ControlButton>
            <ControlButton
              shortcut="ArrowLeft"
              onClick={() => (isImage ? moveVideo(-1) : skip(-skipSeconds))}
              aria-label={
                isImage ? "Previous image" : `Skip back ${skipStep.name}`
              }
              disabled={isImage ? false : undefined}
            >
              <Label>{isImage ? "←" : `−${skipStep.label}`}</Label>
            </ControlButton>
            {/* Playing and pausing are one action whose meaning follows the
                state, so they are one control rather than two, the way every
                other player draws them. */}
            <ControlButton
              shortcut="Space"
              className="play-button"
              onClick={isImage ? toggleSlideshow : playingBack ? pause : play}
              aria-label={
                isImage
                  ? slideshowPlaying
                    ? "Pause"
                    : "Play"
                  : playingBack
                    ? "Pause"
                    : "Play"
              }
            >
              <span
                className={
                  (isImage ? slideshowPlaying : playingBack)
                    ? "pause-glyph"
                    : "play-glyph"
                }
                aria-hidden="true"
              />
            </ControlButton>
            <ControlButton
              shortcut="ArrowRight"
              onClick={() => (isImage ? moveVideo(1) : skip(skipSeconds))}
              aria-label={
                isImage ? "Next image" : `Skip forward ${skipStep.name}`
              }
            >
              <Label>{isImage ? "→" : `+${skipStep.label}`}</Label>
            </ControlButton>
            <ControlButton
              shortcut="PageDown"
              onClick={() => moveVideo(1)}
              aria-label="Next video"
              disabled={loadingMore}
            >
              <NextIcon />
            </ControlButton>
            <span className="time-display control-label">
              {formatTime(currentTime)} / {formatTime(duration)}
            </span>
            <div className="player-utilities">
              <VideoTags
                video={video}
                shortcut="T"
                onChange={updateVideoTags}
                adding={addingTag}
                onAddingChange={(adding) =>
                  adding ? openTagField() : setAddingTag(false)
                }
              />
              {isImage ? (
                <ControlButton
                  shortcut="S"
                  onClick={toggleSlideshow}
                  aria-label={
                    slideshowPlaying ? "Pause slideshow" : "Play slideshow"
                  }
                  aria-pressed={slideshowPlaying}
                >
                  <Label>{slideshowPlaying ? "Pause" : "Play"}</Label>
                </ControlButton>
              ) : (
                <ControlButton
                  shortcut="S"
                  onClick={toggleSubtitles}
                  disabled={subtitles.length === 0}
                  aria-label="Subtitles"
                  aria-pressed={subtitleIndex >= 0}
                >
                  <Label>CC</Label>
                </ControlButton>
              )}
              {!isImage && subtitles.length > 1 ? (
                <select
                  aria-label="Subtitle track"
                  title="Subtitle track"
                  value={subtitleIndex}
                  onChange={(event) => {
                    selectSubtitle(Number(event.currentTarget.value));
                    focusPlayerShell();
                  }}
                >
                  <option value={-1}>Off</option>
                  {subtitles.map((option, position) => (
                    <option key={option.label + position} value={position}>
                      {option.label}
                    </option>
                  ))}
                </select>
              ) : null}
              {/* Named after the step it is on rather than the one it moves
                  to, like the aspect and loop controls, so the button reads as
                  the setting it is. */}
              <ControlButton
                shortcut="J"
                onClick={cycleSkipStep}
                aria-label={`Skip step: ${skipStepName}`}
                disabled={isImage}
              >
                <Label>{skipStep.label}</Label>
              </ControlButton>
              <span className="labelled-control">
                <select
                  aria-label="Playback speed"
                  aria-keyshortcuts="- ="
                  title="Playback speed"
                  value={speed}
                  disabled={isImage}
                  onChange={(event) => {
                    applySpeed(Number(event.currentTarget.value));
                    focusPlayerShell();
                  }}
                >
                  {SPEEDS.map((value) => (
                    <option key={value} value={value}>
                      {value}×
                    </option>
                  ))}
                </select>
                <KeyHint shortcut="- =" />
              </span>
              <ControlButton
                shortcut="ArrowDown"
                onClick={() => applyVolume(volume - VOLUME_STEP)}
                aria-label="Decrease volume"
              >
                <SpeakerIcon waves={1} />
              </ControlButton>
              <input
                className="volume-slider"
                aria-label="Volume"
                type="range"
                min="0"
                max="100"
                step="5"
                value={volume}
                onChange={(event) =>
                  applyVolume(Number(event.currentTarget.value))
                }
              />
              <ControlButton
                shortcut="ArrowUp"
                onClick={() => applyVolume(volume + VOLUME_STEP)}
                aria-label="Increase volume"
              >
                <SpeakerIcon waves={2} />
              </ControlButton>
              {/* The volumes worth going straight to, each pressed rather than
                  stepped towards, and each lit while the volume is sitting on
                  it. */}
              {VOLUME_PRESETS.map((preset) => (
                <ControlButton
                  key={preset.key}
                  shortcut={preset.key}
                  onClick={() => applyVolume(preset.volume)}
                  aria-label={preset.name}
                  aria-pressed={volume === preset.volume}
                >
                  {preset.label ? (
                    <Label>{preset.label}</Label>
                  ) : (
                    <SpeakerIcon waves={0} />
                  )}
                </ControlButton>
              ))}
              <ControlButton
                shortcut="["
                onClick={() => rotate(-90)}
                aria-label="Rotate left"
              >
                <RotateLeftIcon />
              </ControlButton>
              <ControlButton
                shortcut="]"
                onClick={() => rotate(90)}
                aria-label="Rotate right"
              >
                <RotateRightIcon />
              </ControlButton>
              {/* Named after the shape it is currently in, the way the loop
                  control is named after its mode, so the control says what it
                  did as well as what it will do next. The shape is written out
                  rather than drawn: an icon can say that the picture has been
                  reshaped, but only the numbers say which of eight shapes it
                  landed on, which is the whole question while cycling. */}
              <ControlButton
                shortcut="A"
                className={
                  aspectSides
                    ? "transport-button aspect-button on"
                    : "transport-button aspect-button"
                }
                onClick={cycleAspect}
                aria-label={`Aspect ratio: ${aspect}`}
              >
                <Label>{aspect}</Label>
              </ControlButton>
              <ControlButton
                shortcut="L"
                className={
                  loop === "off"
                    ? "transport-button loop-button"
                    : "transport-button loop-button on"
                }
                onClick={cycleLoop}
                aria-label={LOOP_LABELS[loop]}
              >
                <LoopIcon single={loop === "one"} />
              </ControlButton>
              {/* The heading's playlist toggle is out of reach in fullscreen,
                  which is exactly where the drawer has to be summoned, so the
                  overlay carries the control and its shortcut too. */}
              <ControlButton
                shortcut="P"
                onClick={togglePlaylist}
                aria-label="Playlist"
                aria-expanded={drawerOpen}
              >
                <PlaylistIcon />
              </ControlButton>
              <ControlButton
                shortcut="R"
                className="transport-button playlist-button"
                onClick={shufflePlaylist}
                aria-label="Shuffle playlist"
              >
                <Label>Shuffle</Label>
              </ControlButton>
              <ControlButton
                shortcut="F"
                onClick={cycleFullscreen}
                aria-label={fullscreen ? "Exit fullscreen" : "Enter fullscreen"}
              >
                {fullscreen ? <FullscreenExitIcon /> : <FullscreenEnterIcon />}
              </ControlButton>
              {/* Windowed, the heading already names the file and its folder
                  and the transport already shows the clock, so the toggle has
                  nothing left to reveal. */}
              {fullscreen ? (
                <ControlButton
                  shortcut="I"
                  onClick={() => setShowFullscreenInfo((visible) => !visible)}
                  aria-label={
                    showFullscreenInfo
                      ? "Hide fullscreen information"
                      : "Show fullscreen information"
                  }
                  aria-pressed={showFullscreenInfo}
                >
                  <Label>Info</Label>
                </ControlButton>
              ) : null}
              <ControlButton
                shortcut="Shift+Delete"
                onClick={() => void removeCurrentVideo()}
                aria-label="Delete video"
              >
                <Label>Delete</Label>
              </ControlButton>
              {deletedVideo ? (
                <ControlButton
                  shortcut="Ctrl+Shift+Delete"
                  onClick={() => void restoreDeletedVideo()}
                  aria-label="Undo delete"
                >
                  <Label>Undo</Label>
                </ControlButton>
              ) : null}
            </div>
          </div>
        </div>
        {drawerOpen ? (
          <aside
            ref={playlistDrawer}
            className="playlist-drawer"
            aria-label="Playlist"
            onMouseEnter={() => {
              pointerOverPlaylist.current = true;
            }}
            onMouseLeave={() => {
              pointerOverPlaylist.current = false;
              // A drawer the keyboard held open goes back to behaving like one
              // the pointer summoned once the pointer has actually visited it,
              // so walking away from it is enough to dismiss it.
              setFullscreenPlaylist((state) =>
                state === "held" ? "peek" : state,
              );
            }}
          >
            <h2>Up next</h2>
            <ol>
              {playlist.map((item, itemIndex) => (
                <li key={item.id}>
                  <button
                    type="button"
                    className={itemIndex === index ? "active" : undefined}
                    aria-current={itemIndex === index ? "true" : undefined}
                    title={`Play ${item.fileName}`}
                    onClick={() => selectVideo(itemIndex)}
                  >
                    <span className="playlist-marker" />
                    <span>{item.fileName}</span>
                  </button>
                </li>
              ))}
            </ol>
          </aside>
        ) : null}
      </div>
      <p className="playlist-status" aria-live="polite">
        Playlist video {index + 1} of {playlist.length}
        {canLoadMore ? "+" : ""}
      </p>
      {loadingMore ? (
        <p className="message" aria-live="polite">
          Loading next video…
        </p>
      ) : null}
    </section>
  );
}

const RESULT_PAGE_CONCURRENCY = 4;

async function loadResultPages(
  query: string,
  firstPage: number,
  lastPage: number,
  fields: SearchFields,
  mediaType: MediaType,
): Promise<VideoResult[][]> {
  const pages = Array.from(
    { length: Math.max(0, lastPage - firstPage + 1) },
    () => [] as VideoResult[],
  );
  let nextPage = 0;
  const loadWorker = async () => {
    while (nextPage < pages.length) {
      const pageIndex = nextPage++;
      const response = await searchVideos(
        query,
        firstPage + pageIndex,
        fields,
        mediaType,
      );
      pages[pageIndex] = response.results;
    }
  };
  await Promise.all(
    Array.from(
      { length: Math.min(RESULT_PAGE_CONCURRENCY, pages.length) },
      loadWorker,
    ),
  );
  return pages;
}

export default function App() {
  const [showFolderSetup, setShowFolderSetup] = useState(false);
  const [managedIndex, setManagedIndex] = useState<IndexState>();
  const [query, setQuery] = useState("");
  const [fields, setFields] = useState<SearchFields>(DEFAULT_SEARCH_FIELDS);
  const [mediaType, setMediaType] = useState<MediaType>(DEFAULT_MEDIA_TYPE);
  const [page, setPage] = useState<SearchPage>();
  // Playback is always a playlist: choosing one result starts the whole page of
  // them, positioned at the one that was chosen.
  const [playing, setPlaying] = useState<{
    videos: VideoResult[];
    startIndex: number;
  }>();
  const [players, setPlayers] = useState<ExternalPlayer[]>([]);
  const [chosenPlayer, setChosenPlayer] = useState<string>();
  const searchField = useRef<HTMLInputElement>(null);
  // `autoFocus` fires once per mount, and this form is mounted for the whole
  // session — the player renders beside it rather than in its place. Claiming
  // the field whenever no video is playing covers the first paint and every
  // return from one alike, so the next thing typed is always a search.
  //
  // Without `preventScroll` this focus scrolls the field into view, and the
  // field sits above everything a viewer coming back to a long list of results
  // had scrolled past — it would drag them to the top of the page again.
  useEffect(() => {
    if (!playing) searchField.current?.focus({ preventScroll: true });
  }, [playing]);
  const [loadingMore, setLoadingMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const [folderError, setFolderError] = useState<string>();
  // Tagging the whole page of results: whether its field is open, what has been
  // typed into it, whether the renames are still running, and what they did.
  const [taggingAll, setTaggingAll] = useState(false);
  const [tagAllDraft, setTagAllDraft] = useState("");
  const [tagAllBusy, setTagAllBusy] = useState(false);
  const [tagAllOutcome, setTagAllOutcome] = useState<string>();
  const tagAllField = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (taggingAll) tagAllField.current?.focus();
  }, [taggingAll]);
  const requestNumber = useRef(0);
  // State, not a ref: the results list unmounts for as long as a video plays,
  // so coming back puts a brand new marker on screen. A ref would leave the
  // observer watching the detached original — which can never intersect again —
  // and infinite scroll would quietly stop after the first video. Holding the
  // node in state re-runs the effect for whichever marker is really rendered.
  const [loadMoreMarker, setLoadMoreMarker] = useState<HTMLDivElement | null>(
    null,
  );
  const [thumbnailSize, setThumbnailSize] = useState(THUMBNAIL_SIZE_DEFAULT);
  const increaseThumbnailSize = () =>
    setThumbnailSize((size) =>
      Math.min(THUMBNAIL_SIZE_MAX, size + THUMBNAIL_SIZE_STEP),
    );
  const decreaseThumbnailSize = () =>
    setThumbnailSize((size) =>
      Math.max(THUMBNAIL_SIZE_MIN, size - THUMBNAIL_SIZE_STEP),
    );

  // Playing a video takes the results off screen, so the page is left only as
  // tall as the player and a deep scroll offset has nowhere to survive: coming
  // back rendered the list from the top, however far into it the viewer had got.
  // The offset is read while the list is still on screen and put back once it
  // returns. Every page the viewer had loaded is still in `page`, so the list
  // comes back the same height it went away — and where it cannot, `scrollTo`
  // clamps to whatever is really rendered rather than scrolling past the end.
  const resultsScrollOffset = useRef(0);
  // Only a viewer coming back from a video is put back where they were. A fresh
  // search answers a new question, and belongs at the top of its own results.
  const restoreResultsScroll = useRef(false);
  // Held in state for the same reason as the marker above: the list unmounts
  // while a video plays, so the offset has to be restored on whichever list
  // node is really rendered, in a layout effect that runs before it is painted.
  const [resultsList, setResultsList] = useState<HTMLElement | null>(null);
  useLayoutEffect(() => {
    if (!resultsList || !restoreResultsScroll.current) return;
    restoreResultsScroll.current = false;
    // A list left at the top is already where it belongs.
    if (resultsScrollOffset.current)
      window.scrollTo({ top: resultsScrollOffset.current });
  }, [resultsList]);

  // The query the results on screen answer to, which is not always what the
  // field holds: changing what is searched has to search for that again rather
  // than for whatever has been typed since.
  const searchedQuery = useRef<string | undefined>(undefined);

  const runSearch = async (submittedQuery: string, requestedPage: number) => {
    const trimmed = submittedQuery.trim();
    if (!trimmed) return;
    searchedQuery.current = trimmed;
    const currentRequest = ++requestNumber.current;
    setLoading(true);
    setError(undefined);
    setPlaying(undefined);
    try {
      const response = await searchVideos(
        trimmed,
        requestedPage,
        fields,
        mediaType,
      );
      if (currentRequest === requestNumber.current) setPage(response);
    } catch (reason) {
      if (currentRequest === requestNumber.current) {
        setPage(undefined);
        setError(errorMessage(reason));
      }
    } finally {
      if (currentRequest === requestNumber.current) setLoading(false);
    }
  };

  const loadMore = async () => {
    if (!page || loadingMore || page.results.length >= page.totalResults)
      return;
    const loadedPages = page.page;
    if (loadedPages >= page.totalPages) return;
    setLoadingMore(true);
    try {
      const next = await searchVideos(
        page.query,
        loadedPages + 1,
        fields,
        mediaType,
      );
      setPage((current) =>
        current && current.query === next.query
          ? { ...next, results: [...current.results, ...next.results] }
          : current,
      );
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setLoadingMore(false);
    }
  };

  const loadMorePlaylist = async (): Promise<VideoResult[]> => {
    if (!page || page.results.length >= page.totalResults) return [];
    const loadedPage = Math.max(1, page.page);
    if (loadedPage >= Math.max(1, page.totalPages || 1)) return [];
    const next = await searchVideos(
      page.query,
      loadedPage + 1,
      fields,
      mediaType,
    );
    setPage((current) =>
      current && current.query === next.query && current.page === loadedPage
        ? { ...next, results: [...current.results, ...next.results] }
        : current,
    );
    return next.results;
  };

  useEffect(() => {
    if (!page || !loadMoreMarker || typeof IntersectionObserver === "undefined")
      return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) void loadMore();
      },
      { rootMargin: "400px" },
    );
    observer.observe(loadMoreMarker);
    return () => observer.disconnect();
  }, [page, loadingMore, loadMoreMarker]);

  // Asked once: which players are installed does not change while Toka runs,
  // and a computer with none should never be offered the control at all.
  useEffect(() => {
    void externalPlayers()
      .then(setPlayers)
      .catch(() => setPlayers([]));
  }, []);

  // Whichever player is picked, or the first Toka found if the viewer has not
  // picked one.
  const activePlayer = chosenPlayer ?? players[0]?.command;

  // The desktop launcher raises the Toka that is already running rather than
  // starting a second one, so watching two things at once has to be asked for
  // from inside the first.
  const startNewWindow = async () => {
    setError(undefined);
    try {
      await openNewWindow();
    } catch (reason) {
      setError(errorMessage(reason));
    }
  };

  const openInPlayer = async () => {
    if (!page || !activePlayer) return;
    setError(undefined);
    try {
      await openInExternalPlayer(
        activePlayer,
        page.results.map((video) => video.id),
      );
    } catch (reason) {
      setError(errorMessage(reason));
    }
  };

  // The last field left on stays on: a search with nothing to match against
  // could only ever answer "no videos", which reads as a broken search rather
  // than as a setting.
  const onlySearchedField = (field: keyof SearchFields) =>
    fields[field] &&
    SEARCH_SCOPES.every(
      (scope) => scope.field === field || !fields[scope.field],
    );

  const toggleField = (field: keyof SearchFields) => {
    if (onlySearchedField(field)) return;
    setFields((current) => ({ ...current, [field]: !current[field] }));
  };

  // Results on screen were found by looking at particular parts of a video, so
  // changing which parts those are asks the question again rather than leaving
  // an answer that no longer matches the controls.
  const searchedFields = useRef(fields);
  useEffect(() => {
    if (searchedFields.current === fields) return;
    searchedFields.current = fields;
    if (searchedQuery.current) void runSearch(searchedQuery.current, 1);
  });
  const searchedMediaType = useRef(mediaType);
  useEffect(() => {
    if (searchedMediaType.current === mediaType) return;
    searchedMediaType.current = mediaType;
    if (searchedQuery.current) void runSearch(searchedQuery.current, 1);
  });

  // Focus starts in the search field and is put back there on the way out of a
  // video, but anything clicked in between — a tile, a tag, a scope switch —
  // takes it away, and a long page of results leaves the field scrolled off the
  // top. This is the way back to it from wherever the keyboard has ended up.
  useEffect(() => {
    if (playing) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.metaKey || event.altKey || event.shiftKey)
        return;
      if (event.key.toLowerCase() !== "k") return;
      // WebKit reads Ctrl+K in a text field as "delete to end of line".
      event.preventDefault();
      const field = searchField.current;
      if (!field) return;
      // Scrolling to the field is the point here, unlike the focus that
      // follows a video: someone reaching for the search box wants to see it.
      field.focus();
      // Selected rather than merely focused, so the next thing typed asks a
      // new question instead of being appended to the old one.
      field.select();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  // The search field holds the keyboard on this screen, so a bare letter would
  // be typed into it rather than reaching the app; Ctrl is what gets a shortcut
  // through, beside the Ctrl+O that hands a search to another player.
  useEffect(() => {
    if (playing) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.metaKey || event.altKey || event.shiftKey)
        return;
      const scope = SEARCH_SCOPES.find(
        (candidate) => candidate.key.toLowerCase() === event.key.toLowerCase(),
      );
      if (!scope) return;
      event.preventDefault();
      toggleField(scope.field);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  useEffect(() => {
    if (playing || !activePlayer) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.metaKey || event.altKey) return;
      if (event.key.toLowerCase() !== "o") return;
      event.preventDefault();
      void openInPlayer();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.metaKey || event.altKey || event.shiftKey)
        return;
      if (event.key.toLowerCase() !== "n") return;
      event.preventDefault();
      void startNewWindow();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  // Shift is what separates this from the Ctrl+T that chooses whether tags are
  // searched: one asks a question about tags, the other writes them.
  useEffect(() => {
    if (playing || !page?.results.length) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.ctrlKey || !event.shiftKey || event.metaKey || event.altKey)
        return;
      if (event.key.toLowerCase() !== "t") return;
      event.preventDefault();
      setTaggingAll(true);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  // Thumbnail size: the grid fills the width with as many minmax columns as fit.
  // Larger thumbs mean fewer per row, smaller mean more. Ctrl with the window's
  // zoom keys also works, so a viewer reaching for the browser's zoom gets this
  // instead while results are showing.
  useEffect(() => {
    if (playing || !page?.results.length) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (acceptsTypedText(event.target)) return;
      if (event.key === "+" || event.key === "=") {
        event.preventDefault();
        increaseThumbnailSize();
        return;
      }
      if (event.key === "-" || event.key === "_") {
        event.preventDefault();
        decreaseThumbnailSize();
        return;
      }
      // Ctrl and Cmd variants: the key is still + / - / = while the modifier is
      // held, so the handlers above already cover them; no separate branch is
      // needed except to stop the browser's own zoom.
      if (
        (event.ctrlKey || event.metaKey) &&
        (event.key === "Add" || event.key === "Subtract")
      ) {
        event.preventDefault();
        if (event.key === "Add") increaseThumbnailSize();
        else decreaseThumbnailSize();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  useEffect(() => {
    if (playing) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.metaKey || event.altKey || event.shiftKey)
        return;
      if (event.key === "1") {
        event.preventDefault();
        setMediaType("videos");
      } else if (event.key === "2") {
        event.preventDefault();
        setMediaType("images");
      } else if (event.key === "3") {
        event.preventDefault();
        setMediaType("both");
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  // Every video the search matched, not only the pages scrolled to so far:
  // "tag all" means all of them, however far down the list the viewer has got.
  // Playback stays lazy so a large result set does not have to be materialized
  // before the first video can start.
  const everyResult = async (): Promise<VideoResult[]> => {
    if (!page) return [];
    const totalPages = Math.max(1, page.totalPages || 1);
    const loadedPages = Math.max(1, page.page);
    const needsAdditionalPages =
      page.results.length < page.totalResults && totalPages > loadedPages;
    const pages = needsAdditionalPages
      ? await loadResultPages(
          page.query,
          loadedPages + 1,
          totalPages,
          fields,
          mediaType,
        )
      : [];
    return [page.results, ...pages].flat();
  };

  const playSearchResults = (position: number) => {
    if (!page) return;
    resultsScrollOffset.current = window.scrollY;
    setError(undefined);
    setPlaying({ videos: page.results, startIndex: position });
  };

  // One entry, every result. The videos off the bottom of the list are tagged
  // too, so what comes back matches what the summary above the grid claims was
  // found — and the results on screen carry the new names, because a tag is a
  // rename.
  const tagEveryResult = async () => {
    const entry = tagAllDraft.trim();
    if (!page || !entry) return;
    setTaggingAll(false);
    setTagAllDraft("");
    setTagAllOutcome(undefined);
    setTagAllBusy(true);
    setError(undefined);
    try {
      const videos = await everyResult();
      const update = await addTagsToVideos(
        videos.map((video) => video.id),
        [entry],
      );
      const tagged = new Map(
        update.tagged.map((video) => [video.resultId, video]),
      );
      setPage((current) =>
        current
          ? {
              ...current,
              results: current.results.map((video) => {
                const change = tagged.get(video.id);
                return change
                  ? { ...video, fileName: change.fileName, tags: change.tags }
                  : video;
              }),
            }
          : current,
      );
      setTagAllOutcome(taggingOutcome(update, entry));
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setTagAllBusy(false);
    }
  };

  // A playlist — or single video, or folder — Toka was launched with plays
  // as soon as the window is up. Its entries arrive as a page of results, so
  // everything a search's results reach — the player, its drawer, prev and
  // next, coming back to the list — works without a second way through any
  // of it. The launch clears any existing playlist and search term, and a
  // folder's videos arrive shuffled like any other playlist.
  useEffect(() => {
    let cancelled = false;
    void Promise.allSettled([launchPlaylist(), indexState()]).then(
      ([playlistResult, indexResult]) => {
        if (cancelled) return;
        const launched =
          playlistResult.status === "fulfilled" ? playlistResult.value : null;
        if (playlistResult.status === "rejected")
          setError(errorMessage(playlistResult.reason));
        if (indexResult.status === "fulfilled") {
          setManagedIndex(indexResult.value);
          if (
            !launched?.results.length &&
            indexResult.value.supported &&
            indexResult.value.folders.length === 0
          )
            setShowFolderSetup(true);
        }
        // A viewer who got a search in first has asked for something newer than
        // the command line did, and keeps it.
        if (!launched?.results.length || requestNumber.current) return;
        setQuery("");
        setPage(launched);
        setPlaying({ videos: launched.results, startIndex: 0 });
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!managedIndex?.supported) return;
    const poll = window.setInterval(() => {
      void indexState()
        .then(setManagedIndex)
        .catch(() => {});
    }, 2_000);
    return () => window.clearInterval(poll);
  }, [managedIndex?.supported]);

  const chooseIndexFolder = async () => {
    setFolderError(undefined);
    try {
      const path = await open({ directory: true, multiple: false });
      if (typeof path === "string") setManagedIndex(await addIndexFolder(path));
    } catch (reason) {
      setFolderError(errorMessage(reason));
    }
  };

  const forgetIndexFolder = async (id: string) => {
    setFolderError(undefined);
    try {
      setManagedIndex(await removeIndexFolder(id));
    } catch (reason) {
      setFolderError(errorMessage(reason));
    }
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (playing || event.metaKey || event.altKey) return;
      if (
        showFolderSetup &&
        event.ctrlKey &&
        !event.shiftKey &&
        event.key.toLowerCase() === "o"
      ) {
        event.preventDefault();
        void chooseIndexFolder();
      } else if (
        showFolderSetup &&
        managedIndex?.folders.length &&
        event.ctrlKey &&
        !event.shiftKey &&
        event.key === "Enter"
      ) {
        event.preventDefault();
        setShowFolderSetup(false);
      } else if (
        managedIndex?.supported &&
        event.ctrlKey &&
        !event.shiftKey &&
        event.key === ","
      ) {
        event.preventDefault();
        setShowFolderSetup(true);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  const submit = (event: FormEvent) => {
    event.preventDefault();
    void runSearch(query, 1);
  };

  const updateTags = (
    videoId: string,
    update: Pick<VideoResult, "fileName" | "tags">,
  ) => {
    setPage((current) =>
      current
        ? {
            ...current,
            results: current.results.map((video) =>
              video.id === videoId ? { ...video, ...update } : video,
            ),
          }
        : current,
    );
  };

  const hasSubmitted =
    loading || Boolean(page) || Boolean(error) || Boolean(playing);
  const indexHasAdvanced = Boolean(
    page?.indexRevision !== undefined &&
    managedIndex?.supported &&
    managedIndex.revision > page.indexRevision,
  );

  useEffect(() => {
    if (!indexHasAdvanced || playing) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (
        !event.ctrlKey ||
        !event.shiftKey ||
        event.metaKey ||
        event.altKey ||
        event.key.toLowerCase() !== "r"
      )
        return;
      event.preventDefault();
      if (page) void runSearch(page.query, 1);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });
  // The same name on the control and on the field it opens, so a viewer who
  // reached one by keyboard is still in the same place after it opens.
  const tagAllLabel = `Tag all ${page?.totalResults ?? 0} videos`;
  const resultNoun = mediaType === "images" ? "image" : "video";

  if (showFolderSetup && !playing) {
    return (
      <main className="app folder-setup">
        <section aria-label="Search folder setup">
          <h1>Choose where Toka searches</h1>
          <p>
            Add the folders that contain your videos and images. Toka keeps
            their search indexes up to date in the background.
          </p>
          <ControlButton
            shortcut="Ctrl+O"
            className="scope-toggle"
            aria-label="Add folder"
            onClick={() => void chooseIndexFolder()}
          >
            <Label>Add folder</Label>
          </ControlButton>
          {managedIndex?.folders.length ? (
            <ul className="folder-list">
              {managedIndex.folders.map((folder) => (
                <li key={folder.id}>
                  <span className="folder-path">{folder.path}</span>
                  <span className={`folder-status ${folder.status}`}>
                    {folder.status === "pending"
                      ? "Waiting to index"
                      : folder.status === "indexing"
                        ? "Indexing…"
                        : folder.status === "ready"
                          ? "Ready"
                          : folder.status === "offline"
                            ? "Drive disconnected"
                            : (folder.message ?? "Indexing failed")}
                  </span>
                  <ControlButton
                    shortcut="Delete"
                    className="scope-toggle"
                    aria-label={`Remove ${folder.path}`}
                    onClick={() => void forgetIndexFolder(folder.id)}
                  >
                    <Label>Remove</Label>
                  </ControlButton>
                </li>
              ))}
            </ul>
          ) : null}
          {folderError ? (
            <p role="alert" className="message error">
              {folderError}
            </p>
          ) : null}
          <ControlButton
            shortcut="Ctrl+Enter"
            className="scope-toggle"
            aria-label="Start searching"
            disabled={!managedIndex?.folders.length}
            onClick={() => setShowFolderSetup(false)}
          >
            <Label>Start searching</Label>
          </ControlButton>
        </section>
      </main>
    );
  }

  return (
    <main className={hasSubmitted ? "app" : "app initial"}>
      <form role="search" onSubmit={submit} className="search-form">
        <label className="sr-only" htmlFor="video-search">
          Search videos
        </label>
        <div className="search-field">
          <input
            id="video-search"
            type="search"
            value={query}
            onChange={(event) => setQuery(event.currentTarget.value)}
            placeholder="Search videos…"
            autoComplete="off"
            aria-keyshortcuts={SEARCH_SHORTCUT}
            ref={searchField}
          />
          {/* The field is its own control, so the hint sits in it rather than
              on a button that would do no more than a click on the field
              already does. It still reads its text from the value the
              shortcut is declared with, so the two cannot drift apart. */}
          <KeyHint shortcut={SEARCH_SHORTCUT} />
          {query ? (
            <button
              type="button"
              className="clear-search"
              aria-label="Clear search"
              title="Clear search"
              onClick={() => setQuery("")}
            >
              ×
            </button>
          ) : (
            <SearchIcon />
          )}
        </div>
        {/* Where a search looks. Any combination will do, so these are three
            independent switches rather than a cycle through the seven states
            they can be in between them. */}
        <div className="search-scope" role="group" aria-label="Search in">
          {SEARCH_SCOPES.map((scope) => (
            <ControlButton
              key={scope.field}
              shortcut={`Ctrl+${scope.key}`}
              className="scope-toggle"
              aria-label={`Search ${scope.name}`}
              aria-pressed={fields[scope.field]}
              disabled={onlySearchedField(scope.field)}
              title={
                onlySearchedField(scope.field)
                  ? `A search has to look somewhere, so ${scope.name} stays on`
                  : `Search ${scope.name}`
              }
              onClick={() => toggleField(scope.field)}
            >
              <Label>{scope.name}</Label>
            </ControlButton>
          ))}
        </div>
        <div className="search-scope" role="group" aria-label="Media type">
          {MEDIA_TYPES.map((media) => (
            <ControlButton
              key={media.value}
              shortcut={media.shortcut}
              className="scope-toggle"
              aria-label={media.label}
              aria-pressed={mediaType === media.value}
              onClick={() => setMediaType(media.value)}
            >
              <Label>{media.label}</Label>
            </ControlButton>
          ))}
        </div>
        {/* Always here rather than beside the results: a second Toka is worth
            asking for before there is anything to show. */}
        <div className="search-actions">
          {managedIndex?.supported ? (
            <ControlButton
              shortcut="Ctrl+,"
              className="scope-toggle"
              aria-label="Search folders"
              onClick={() => setShowFolderSetup(true)}
            >
              <Label>Search folders</Label>
            </ControlButton>
          ) : null}
          <ControlButton
            shortcut="Ctrl+N"
            className="scope-toggle"
            aria-label="New window"
            onClick={() => void startNewWindow()}
          >
            <Label>New window</Label>
          </ControlButton>
        </div>
      </form>

      {!hasSubmitted ? (
        <section className="build-info" aria-label="Build information">
          <span>Version {buildInfo.version}</span>
          <span>Built {buildInfo.builtAt}</span>
          <span>Git SHA {buildInfo.gitSha}</span>
        </section>
      ) : null}

      {loading ? (
        <p className="message" aria-live="polite">
          Searching…
        </p>
      ) : null}
      {error ? (
        <p role="alert" className="message error">
          {error}
        </p>
      ) : null}
      {indexHasAdvanced && page && !playing ? (
        <aside className="index-refresh" aria-live="polite">
          <span>New files are ready to search.</span>
          <ControlButton
            shortcut="Ctrl+Shift+R"
            className="scope-toggle"
            aria-label="Refresh search results"
            onClick={() => void runSearch(page.query, 1)}
          >
            <Label>Refresh</Label>
          </ControlButton>
        </aside>
      ) : null}
      {playing && page ? (
        <Player
          videos={playing.videos}
          startIndex={playing.startIndex}
          hasMore={page.results.length < page.totalResults}
          onLoadMore={loadMorePlaylist}
          onBack={() => {
            restoreResultsScroll.current = true;
            setPlaying(undefined);
          }}
          onTagsChange={updateTags}
        />
      ) : null}

      {!loading && !playing && page ? (
        <section className="results" ref={setResultsList}>
          <div className="results-summary">
            <p>
              {page.totalResults}{" "}
              {page.totalResults === 1 ? resultNoun : `${resultNoun}s`}
            </p>
            {page.results.length > 1 ? (
              <button
                type="button"
                className="playlist-button"
                title="Play all videos"
                onClick={() => playSearchResults(0)}
              >
                <Label>Play all</Label>
              </button>
            ) : null}
            {page.results.length > 1 ? (
              <ControlButton
                shortcut="R"
                className="playlist-button"
                aria-label="Shuffle results"
                onClick={() =>
                  setPage((current) =>
                    current
                      ? { ...current, results: shuffleVideos(current.results) }
                      : current,
                  )
                }
              >
                <Label>Shuffle</Label>
              </ControlButton>
            ) : null}
            {/* One tag for the whole search, so a folder's worth of videos does
                not have to be tagged a tile at a time. The count is in the name
                rather than only in the summary beside it: this renames every
                video the search matched, including the ones off the bottom of
                the list, and that is worth being unambiguous about. */}
            {page.results.length > 1 ? (
              taggingAll ? (
                <input
                  ref={tagAllField}
                  className="tag-all-field"
                  aria-label={tagAllLabel}
                  value={tagAllDraft}
                  onChange={(event) =>
                    setTagAllDraft(event.currentTarget.value)
                  }
                  onKeyDown={(event) => {
                    if (event.key === "Enter") void tagEveryResult();
                    if (event.key === "Escape") setTaggingAll(false);
                  }}
                />
              ) : (
                <ControlButton
                  shortcut="Ctrl+Shift+T"
                  className="playlist-button"
                  aria-label={tagAllLabel}
                  disabled={tagAllBusy}
                  onClick={() => setTaggingAll(true)}
                >
                  <Label>{tagAllBusy ? "Tagging…" : "Tag all"}</Label>
                </ControlButton>
              )
            ) : null}
            {/* Only offered where there is something to offer: a computer with
                no other video player installed gets no control at all. */}
            {players.length ? (
              <span className="labelled-control">
                <select
                  aria-label="Video player"
                  title="Video player"
                  value={activePlayer}
                  onChange={(event) =>
                    setChosenPlayer(event.currentTarget.value)
                  }
                >
                  {players.map((player) => (
                    <option key={player.command} value={player.command}>
                      {player.name}
                    </option>
                  ))}
                </select>
                <ControlButton
                  shortcut="Ctrl+O"
                  className="playlist-button"
                  aria-label="Open in player"
                  onClick={() => void openInPlayer()}
                >
                  <Label>Open in</Label>
                </ControlButton>
              </span>
            ) : null}
            <p>
              {page.results.length} of {page.totalResults} loaded
            </p>
          </div>
          {tagAllOutcome ? (
            <p
              className="message"
              role="status"
              aria-label="Tagging results"
              aria-live="polite"
            >
              {tagAllOutcome}
            </p>
          ) : null}
          <div
            className="thumbnail-controls"
            role="group"
            aria-label="Thumbnail size"
            style={{
              display: "flex",
              gap: "8px",
              justifyContent: "flex-end",
              marginBottom: "12px",
            }}
          >
            <ControlButton
              shortcut="-"
              className="scope-toggle"
              aria-label="Smaller thumbnails"
              onClick={decreaseThumbnailSize}
              disabled={thumbnailSize <= THUMBNAIL_SIZE_MIN}
            >
              <ThumbnailSizeIcon larger={false} />
            </ControlButton>
            <ControlButton
              shortcut="+"
              className="scope-toggle"
              aria-label="Larger thumbnails"
              onClick={increaseThumbnailSize}
              disabled={thumbnailSize >= THUMBNAIL_SIZE_MAX}
            >
              <ThumbnailSizeIcon larger />
            </ControlButton>
          </div>
          {page.results.length ? (
            <ul
              className="video-grid"
              aria-label="Video results"
              style={
                {
                  "--thumbnail-size": `${thumbnailSize}px`,
                } as CSSProperties
              }
            >
              {page.results.map((video, position) => (
                <li key={video.id}>
                  <VideoTile
                    video={video}
                    onPlay={() => void playSearchResults(position)}
                  />
                  <VideoTags
                    video={video}
                    onChange={(update) => updateTags(video.id, update)}
                  />
                </li>
              ))}
            </ul>
          ) : (
            <p className="message">No matching videos found.</p>
          )}
          {page.results.length < page.totalResults ? (
            <div
              ref={setLoadMoreMarker}
              className="load-more-marker"
              aria-live="polite"
            >
              {loadingMore ? "Loading more videos…" : "Scroll for more videos"}
            </div>
          ) : null}
        </section>
      ) : null}
    </main>
  );
}
