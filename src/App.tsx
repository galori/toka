import {
  ButtonHTMLAttributes,
  CSSProperties,
  FormEvent,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
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
  setNativeVideoBounds,
  stopNativeVideo,
  subtitleCues,
  deleteVideo,
  undoDelete,
  addVideoTags,
  removeVideoTags,
  videoThumbnail,
  type PreparedVideo,
  type SearchPage,
  type VideoResult,
  type VideoTagUpdate,
} from "./api";
import { buildInfo } from "./buildInfo";

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

function VideoThumbnail({ video }: { video: VideoResult }) {
  const container = useRef<HTMLSpanElement>(null);
  const [thumbnailPath, setThumbnailPath] = useState(video.thumbnailPath);

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

  return (
    <span
      ref={container}
      className="video-art"
      style={
        thumbnailPath
          ? { backgroundImage: `url(${convertFileSrc(thumbnailPath)})` }
          : undefined
      }
    >
      {thumbnailPath ? (
        <span className="thumbnail-overlay" aria-hidden="true" />
      ) : (
        <VideoIcon />
      )}
    </span>
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

// How long the fullscreen overlay waits after the last movement before fading.
const CONTROLS_IDLE_DELAY = 2_500;

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

const SPEEDS = [0.1, 0.25, 0.5, 0.75, 1, 1.25, 1.5, 2, 3, 4];
const VOLUME_STEP = 5;

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
  PageUp: "PgUp",
  PageDown: "PgDn",
};

function KeyHint({ shortcut }: { shortcut: string }) {
  const label = shortcut
    .split(" ")
    .map((combination) =>
      combination
        .split("+")
        .map((key) => KEY_GLYPHS[key] ?? key)
        .join(""),
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

function prefersWindowFullscreen(): boolean {
  return navigator.userAgent.includes("Mac OS X");
}

function Player({
  videos,
  startIndex,
  onBack,
  onTagsChange,
}: {
  videos: VideoResult[];
  startIndex: number;
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
  const fullscreenPath = useRef<HTMLDivElement>(null);
  const fullscreenTime = useRef<HTMLDivElement>(null);
  const sidecarTracks = useRef<TextTrack[]>([]);
  const [index, setIndex] = useState(startIndex);
  const [prepared, setPrepared] = useState<PreparedVideo>();
  const [duration, setDuration] = useState(0);
  const [currentTime, setCurrentTime] = useState(0);
  const [error, setError] = useState<string>();
  const [fullscreenError, setFullscreenError] = useState<string>();
  const [fullscreen, setFullscreen] = useState(false);
  const [showFullscreenInfo, setShowFullscreenInfo] = useState(true);
  const [controlsIdle, setControlsIdle] = useState(false);
  const [loop, setLoop] = useState<LoopMode>("playlist");
  const [speed, setSpeed] = useState(1);
  const [volume, setVolume] = useState(100);
  const [playingBack, setPlayingBack] = useState(false);
  const [rotation, setRotation] = useState(0);
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
  const [deletedVideo, setDeletedVideo] = useState<{
    video: VideoResult;
    index: number;
  }>();
  const [addingTag, setAddingTag] = useState(false);
  // Mirrored into a ref so the fullscreen idle timer can see it without
  // restarting, the way it already watches the pointer.
  const tagFieldOpen = useRef(false);
  const video = playlist[index];
  useEffect(() => {
    tagFieldOpen.current = addingTag;
  }, [addingTag]);
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
    setError(undefined);
    setFullscreenError(undefined);
    setPlayingBack(false);
    setRotation(0);
    setNativeBaseRotation(0);
    setNativeSubtitles([]);
    setEmbeddedSubtitles([]);
    setSubtitleIndex(-1);
    setSidecarTextTrack(undefined);
    sidecarTracks.current = [];
    prepareVideo(video.id)
      .then(async (result) => {
        if (!active) return;
        if (result.playbackBackend === "native") {
          nativeActive = true;
          await loadNativeVideo(result.filePath);
          const baseRotation = await nativeVideoRotation();
          if (active) setNativeBaseRotation(baseRotation);
          // mpv starts every file at 1x.
          if (chosenSpeed.current !== 1)
            await setNativeSpeed(chosenSpeed.current);
          if (chosenVolume.current !== 100)
            await setNativeVolume(chosenVolume.current);
          await setNativePaused(false);
          if (active) setPlayingBack(true);
        }
        if (active) setPrepared(result);
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

  const native = prepared?.playbackBackend === "native";
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
    if (!native || !nativeSurface.current) return;
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
      const path = fullscreenPath.current?.getBoundingClientRect();
      const time = fullscreenTime.current?.getBoundingClientRect();
      // GTK renders the native GL area above the opaque WebView. Trim its
      // bounds around WebView controls and the playlist so the real decoded
      // picture remains visible without covering those interactive elements.
      // The fullscreen overlay is trimmed around for the same reason: it is
      // read over the picture everywhere else, but on Linux the picture would
      // simply be painted on top of it.
      const top = path
        ? Math.min(bounds.bottom, Math.max(bounds.top, path.bottom))
        : bounds.top;
      const floors = [bounds.bottom];
      if (controls && !controlsIdle) floors.push(controls.top);
      if (time) floors.push(time.top);
      const visibleHeight = Math.max(1, Math.min(...floors) - top);
      const visibleWidth = drawer
        ? Math.max(1, Math.min(bounds.width, drawer.left - bounds.left))
        : bounds.width;
      void setNativeVideoBounds({
        x: Math.round(bounds.x),
        y: Math.round(top),
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
  }, [controlsIdle, drawerOpen, fullscreen, index, native, showFullscreenInfo]);

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
    setIndex(
      (current) => (current + direction + playlist.length) % playlist.length,
    );
  };

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

  // Fullscreen is for watching, so the overlay gets out of the way at once and
  // stays away until the viewer reaches for it — only the scrubber is left, as
  // a sliver along the very bottom. Windowed playback always shows the controls.
  useEffect(() => {
    if (!fullscreen) {
      setControlsIdle(false);
      setFullscreenPlaylist("hidden");
      pointerOverPlaylist.current = false;
      return;
    }
    // Fullscreen is entered by clicking a control or pressing a key, either of
    // which leaves the pointer resting on the overlay it is about to hide; that
    // would otherwise pin the bar open until the viewer moved the mouse away.
    pointerOverControls.current = false;
    setControlsIdle(true);
    let lastActivity = 0;
    const wake = () => {
      lastActivity = Date.now();
      setControlsIdle(false);
    };
    // Polling rather than a one-shot timer so that moving the pointer off the
    // controls re-arms the countdown without needing its own listener.
    // Keyboard use keeps them up through the keydown listener below; focus is
    // deliberately not consulted, because clicking a control focuses it and
    // would then pin the overlay open for the rest of the session.
    const tick = window.setInterval(() => {
      if (pointerOverControls.current || tagFieldOpen.current)
        lastActivity = Date.now();
      else if (Date.now() - lastActivity >= CONTROLS_IDLE_DELAY)
        setControlsIdle(true);
    }, 250);
    window.addEventListener("mousemove", wake);
    document.addEventListener("mousemove", wake);
    document.addEventListener("pointermove", wake);
    window.addEventListener("keydown", wake);
    return () => {
      window.clearInterval(tick);
      window.removeEventListener("mousemove", wake);
      document.removeEventListener("mousemove", wake);
      document.removeEventListener("pointermove", wake);
      window.removeEventListener("keydown", wake);
    };
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
    // Polled for the same reason the controls are: the pointer resting on the
    // drawer has to hold it open without a listener of its own re-arming a
    // one-shot timer on every movement.
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

  const focusPlayerShell = () => {
    playerShell.current?.focus({ preventScroll: true });
  };

  const toggleFullscreen = () => {
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
        (target instanceof Element &&
          target.closest(
            "input, textarea, select, [contenteditable], [role=textbox]",
          ))
      )
        return;

      const run = (action: () => void) => {
        event.preventDefault();
        action();
      };
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
        run(playingBack ? pause : play);
      } else if (event.key === "[") {
        run(() => rotate(-90));
      } else if (event.key === "]") {
        run(() => rotate(90));
      } else if (event.key === "ArrowLeft") {
        run(() => skip(-10));
      } else if (event.key === "ArrowRight") {
        run(() => skip(10));
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
      } else if (event.key === "9") {
        run(() => applyVolume(volume - VOLUME_STEP));
      } else if (event.key === "0") {
        run(() => applyVolume(volume + VOLUME_STEP));
      } else if (event.key.toLowerCase() === "t") {
        run(openTagField);
      } else if (event.key.toLowerCase() === "s" && subtitles.length > 0) {
        run(toggleSubtitles);
      } else if (event.key.toLowerCase() === "l") {
        run(cycleLoop);
      } else if (event.key.toLowerCase() === "p") {
        run(togglePlaylist);
      } else if (event.key.toLowerCase() === "f") {
        run(toggleFullscreen);
      } else if (event.key.toLowerCase() === "i" && fullscreen) {
        run(() => setShowFullscreenInfo((visible) => !visible));
      } else if (event.key === "Escape") {
        run(fullscreen ? toggleFullscreen : onBack);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    currentTime,
    duration,
    fullscreen,
    index,
    native,
    nativeBaseRotation,
    onBack,
    addingTag,
    playingBack,
    playlist.length,
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
          {index < playlist.length - 1 ? (
            <button
              type="button"
              className="playlist-button"
              title="Skip to next video"
              onClick={() => setIndex((current) => current + 1)}
            >
              <Label>Skip to next</Label>
            </button>
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
            <p className="player-file-path" title={prepared.filePath}>
              {prepared.filePath}
            </p>
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
        className={`player-shell${fullscreen ? " fullscreen" : ""}${controlsIdle ? " idle" : ""}`}
        tabIndex={-1}
      >
        {!prepared ? (
          <p className="message preparing-video">Preparing video…</p>
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
            style={{ transform: `rotate(${rotation}deg)` }}
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
        {fullscreen && showFullscreenInfo ? (
          <div
            className="fullscreen-info"
            role="region"
            aria-label="Fullscreen video information"
          >
            <div ref={fullscreenPath} className="fullscreen-file-path">
              {prepared?.filePath ?? video.fileName}
            </div>
            <div ref={fullscreenTime} className="fullscreen-time">
              {formatTime(currentTime)} / {formatTime(duration)}
            </div>
          </div>
        ) : null}
        <div
          ref={playerControls}
          className={controlsIdle ? "player-controls idle" : "player-controls"}
          aria-label="Video controls"
          onMouseEnter={() => {
            pointerOverControls.current = true;
          }}
          onMouseLeave={() => {
            pointerOverControls.current = false;
          }}
        >
          <input
            className="player-timeline"
            aria-label="Video timeline"
            title="Video timeline"
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
              onClick={() => skip(-10)}
              aria-label="Skip back 10 seconds"
            >
              <Label>−10</Label>
            </ControlButton>
            {/* Playing and pausing are one action whose meaning follows the
                state, so they are one control rather than two, the way every
                other player draws them. */}
            <ControlButton
              shortcut="Space"
              className="play-button"
              onClick={playingBack ? pause : play}
              aria-label={playingBack ? "Pause" : "Play"}
            >
              <span
                className={playingBack ? "pause-glyph" : "play-glyph"}
                aria-hidden="true"
              />
            </ControlButton>
            <ControlButton
              shortcut="ArrowRight"
              onClick={() => skip(10)}
              aria-label="Skip forward 10 seconds"
            >
              <Label>+10</Label>
            </ControlButton>
            <ControlButton
              shortcut="PageDown"
              onClick={() => moveVideo(1)}
              aria-label="Next video"
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
              <ControlButton
                shortcut="S"
                onClick={toggleSubtitles}
                disabled={subtitles.length === 0}
                aria-label="Subtitles"
                aria-pressed={subtitleIndex >= 0}
              >
                <Label>CC</Label>
              </ControlButton>
              {subtitles.length > 1 ? (
                <select
                  aria-label="Subtitle track"
                  title="Subtitle track"
                  value={subtitleIndex}
                  onChange={(event) =>
                    selectSubtitle(Number(event.currentTarget.value))
                  }
                >
                  <option value={-1}>Off</option>
                  {subtitles.map((option, position) => (
                    <option key={option.label + position} value={position}>
                      {option.label}
                    </option>
                  ))}
                </select>
              ) : null}
              <span className="labelled-control">
                <select
                  aria-label="Playback speed"
                  aria-keyshortcuts="- ="
                  title="Playback speed"
                  value={speed}
                  onChange={(event) =>
                    applySpeed(Number(event.currentTarget.value))
                  }
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
                shortcut="9"
                onClick={() => applyVolume(volume - VOLUME_STEP)}
                aria-label="Decrease volume"
              >
                <Label>Vol −</Label>
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
                shortcut="0"
                onClick={() => applyVolume(volume + VOLUME_STEP)}
                aria-label="Increase volume"
              >
                <Label>Vol +</Label>
              </ControlButton>
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
                onClick={toggleFullscreen}
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
      </p>
    </section>
  );
}

export default function App() {
  const [query, setQuery] = useState("");
  const [page, setPage] = useState<SearchPage>();
  // Playback is always a playlist: choosing one result starts the whole page of
  // them, positioned at the one that was chosen.
  const [playing, setPlaying] = useState<{
    videos: VideoResult[];
    startIndex: number;
  }>();
  const [playlistLoading, setPlaylistLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const requestNumber = useRef(0);
  const loadMoreMarker = useRef<HTMLDivElement>(null);

  const runSearch = async (submittedQuery: string, requestedPage: number) => {
    const trimmed = submittedQuery.trim();
    if (!trimmed) return;
    const currentRequest = ++requestNumber.current;
    setLoading(true);
    setError(undefined);
    setPlaying(undefined);
    try {
      const response = await searchVideos(trimmed, requestedPage);
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
      const next = await searchVideos(page.query, loadedPages + 1);
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

  useEffect(() => {
    if (
      !page ||
      !loadMoreMarker.current ||
      typeof IntersectionObserver === "undefined"
    )
      return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) void loadMore();
      },
      { rootMargin: "400px" },
    );
    observer.observe(loadMoreMarker.current);
    return () => observer.disconnect();
  }, [page, loadingMore]);

  const playSearchResults = async (position: number) => {
    if (!page) return;
    setPlaylistLoading(true);
    setError(undefined);
    try {
      const totalPages = Math.max(1, page.totalPages || 1);
      const loadedPages = Math.max(1, page.page);
      const needsAdditionalPages =
        page.results.length < page.totalResults && totalPages > loadedPages;
      const pages = needsAdditionalPages
        ? await Promise.all(
            Array.from({ length: totalPages - loadedPages }, (_, offset) =>
              searchVideos(page.query, loadedPages + offset + 1).then(
                (response) => response?.results ?? [],
              ),
            ),
          )
        : [];
      const videos = [page.results, ...pages].flat();
      setPlaying({
        videos,
        startIndex: position,
      });
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setPlaylistLoading(false);
    }
  };

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
            autoFocus
          />
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
      {playing && page ? (
        <Player
          videos={playing.videos}
          startIndex={playing.startIndex}
          onBack={() => setPlaying(undefined)}
          onTagsChange={updateTags}
        />
      ) : null}

      {!loading && !playing && page ? (
        <section className="results">
          <div className="results-summary">
            <p>
              {page.totalResults} {page.totalResults === 1 ? "video" : "videos"}
            </p>
            {page.results.length > 1 ? (
              <button
                type="button"
                className="playlist-button"
                title="Play all videos"
                disabled={playlistLoading}
                onClick={() => void playSearchResults(0)}
              >
                <Label>
                  {playlistLoading ? "Loading playlist…" : "Play all"}
                </Label>
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
            <p>
              {page.results.length} of {page.totalResults} loaded
            </p>
          </div>
          {page.results.length ? (
            <ul className="video-grid" aria-label="Video results">
              {page.results.map((video, position) => (
                <li key={video.id}>
                  <button
                    type="button"
                    className="video-tile"
                    aria-label={`Play ${video.fileName}`}
                    title={`Play ${video.fileName}`}
                    onClick={() => void playSearchResults(position)}
                  >
                    <VideoThumbnail video={video} />
                    <span className="video-name">{video.fileName}</span>
                  </button>
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
              ref={loadMoreMarker}
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
