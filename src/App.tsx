import { ButtonHTMLAttributes, FormEvent, useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
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
  setNativeSubtitle,
  setNativeVideoRotation,
  setNativeVideoBounds,
  stopNativeVideo,
  subtitleCues,
  type PreparedVideo,
  type SearchPage,
  type VideoResult,
} from "./api";

// A subtitle Toka can turn on, whichever backend supplies it: a sidecar file
// detected by Rust, an mpv track, or a track the web engine found in the file.
type SubtitleOption =
  | { source: "sidecar"; label: string; language: string | null; track: number }
  | { source: "native"; label: string; id: number }
  | { source: "embedded"; label: string; textTrack: TextTrack };

function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error && typeof error === "object" && "message" in error) {
    return String(error.message);
  }
  return "Something went wrong while searching for videos.";
}

function VideoIcon() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden="true" className="video-icon">
      <rect x="7" y="12" width="50" height="40" rx="8" />
      <path d="m27 23 16 9-16 9Z" />
    </svg>
  );
}

// How long the fullscreen overlay waits after the last movement before fading.
const CONTROLS_IDLE_DELAY = 2_500;

const SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 2];

// DOM key names are what `aria-keyshortcuts` wants; these are what a viewer
// should read on the button itself.
const KEY_GLYPHS: Record<string, string> = {
  Escape: "Esc",
  Space: "Space",
  ArrowLeft: "←",
  ArrowRight: "→",
  Shift: "⇧",
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
  return <span className="key-hint" aria-hidden="true">{label}</span>;
}

// Pairs the declared shortcut with the one shown on the control, so the two
// cannot drift apart as bindings change.
function ControlButton({
  shortcut,
  className = "transport-button",
  children,
  ...rest
}: { shortcut: string } & ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button type="button" className={className} aria-keyshortcuts={shortcut} {...rest}>
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
  if (import.meta.env.VITE_E2E === "1" && navigator.userAgent.includes("Linux")) {
    const fileName = filePath.split(/[\\/]/).at(-1) ?? "";
    return `http://127.0.0.1:1421/${encodeURIComponent(fileName)}`;
  }
  return convertFileSrc(filePath);
}

function Player({ videos, onBack }: { videos: VideoResult[]; onBack: () => void }) {
  const element = useRef<HTMLVideoElement>(null);
  const playerShell = useRef<HTMLDivElement>(null);
  const playerControls = useRef<HTMLDivElement>(null);
  const pointerOverControls = useRef(false);
  const nativeSurface = useRef<HTMLDivElement>(null);
  const [index, setIndex] = useState(0);
  const [prepared, setPrepared] = useState<PreparedVideo>();
  const [duration, setDuration] = useState(0);
  const [currentTime, setCurrentTime] = useState(0);
  const [error, setError] = useState<string>();
  const [fullscreen, setFullscreen] = useState(false);
  const [controlsIdle, setControlsIdle] = useState(false);
  const [loop, setLoop] = useState(false);
  const [speed, setSpeed] = useState(1);
  const [playingBack, setPlayingBack] = useState(false);
  const [rotation, setRotation] = useState(0);
  const [nativeBaseRotation, setNativeBaseRotation] = useState(0);
  const [playlistOpen, setPlaylistOpen] = useState(videos.length > 1);
  const [nativeSubtitles, setNativeSubtitles] = useState<SubtitleOption[]>([]);
  const [embeddedSubtitles, setEmbeddedSubtitles] = useState<SubtitleOption[]>([]);
  const [subtitleIndex, setSubtitleIndex] = useState(-1);
  const [subtitleCueUrl, setSubtitleCueUrl] = useState<string>();
  const video = videos[index];

  useEffect(() => {
    let active = true;
    let nativeActive = false;
    setPrepared(undefined);
    setDuration(0);
    setCurrentTime(0);
    setError(undefined);
    setSpeed(1);
    setPlayingBack(false);
    setRotation(0);
    setNativeBaseRotation(0);
    setNativeSubtitles([]);
    setEmbeddedSubtitles([]);
    setSubtitleIndex(-1);
    setSubtitleCueUrl(undefined);
    prepareVideo(video.id)
      .then(async (result) => {
        if (!active) return;
        if (result.playbackBackend === "native") {
          nativeActive = true;
          await loadNativeVideo(result.filePath);
          const baseRotation = await nativeVideoRotation();
          if (active) setNativeBaseRotation(baseRotation);
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
        void setNativeVideoBounds({ x: 0, y: 0, width: 1, height: 1, visible: false }).catch(() => {});
        void stopNativeVideo().catch(() => {});
      }
    };
  }, [video.id]);

  const native = prepared?.playbackBackend === "native";

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
    // so the list is refreshed alongside the playback poll until it settles.
    let knownTrackCount = -1;
    const updateBounds = () => {
      const bounds = surface.getBoundingClientRect();
      void setNativeVideoBounds({
        x: Math.round(bounds.x),
        y: Math.round(bounds.y),
        width: Math.round(bounds.width),
        height: Math.round(bounds.height),
        visible: bounds.width > 0 && bounds.height > 0,
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
            if (index < videos.length - 1) setIndex((current) => current + 1);
            else if (loop && videos.length > 1) setIndex(0);
            else if (loop) {
              void seekNativeVideo(0)
                .then(() => setNativePaused(false))
                .catch((reason: unknown) => setError(errorMessage(reason)));
            }
          }
        })
        .catch((reason: unknown) => setError(errorMessage(reason)));
      void nativeSubtitleTracks()
        .then((tracks) => {
          if (tracks.length === knownTrackCount) return;
          knownTrackCount = tracks.length;
          setNativeSubtitles(
            tracks.map((track) => ({ source: "native", label: track.label, id: track.id })),
          );
        })
        .catch(() => {});
    }, 250);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", updateBounds);
      window.clearInterval(poll);
    };
  }, [index, loop, native, videos.length]);

  const play = () => {
    if (native) {
      void setNativePaused(false)
        .then(() => setPlayingBack(true))
        .catch((reason: unknown) => setError(errorMessage(reason)));
      return;
    }
    void element.current?.play()
      .then(() => setPlayingBack(true))
      .catch(() => setError("This video could not be played by the system media engine."));
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

  const rotate = (amount: number) => {
    setRotation((current) => {
      const next = (current + amount + 360) % 360;
      if (native) {
        const degrees = (nativeBaseRotation + next) % 360;
        void setNativeVideoRotation(degrees).catch((reason: unknown) => setError(errorMessage(reason)));
      }
      return next;
    });
  };

  const skip = (amount: number) => {
    const next = Math.max(0, Math.min(duration || Number.POSITIVE_INFINITY, currentTime + amount));
    if (native) void seekNativeVideo(next).catch((reason: unknown) => setError(errorMessage(reason)));
    else if (element.current) element.current.currentTime = next;
    setCurrentTime(next);
  };

  const selectVideo = (nextIndex: number) => {
    if (nextIndex >= 0 && nextIndex < videos.length) setIndex(nextIndex);
  };

  const applySpeed = (next: number) => {
    setSpeed(next);
    if (native) void setNativeSpeed(next).catch((reason: unknown) => setError(errorMessage(reason)));
    else if (element.current) element.current.playbackRate = next;
  };

  // Holds at the ends of the range rather than wrapping, so holding the key
  // down cannot jump from slowest straight back to fastest.
  const stepSpeed = (direction: number) => {
    const at = SPEEDS.indexOf(speed);
    const next = SPEEDS[Math.min(SPEEDS.length - 1, Math.max(0, (at < 0 ? SPEEDS.indexOf(1) : at) + direction))];
    if (next !== speed) applySpeed(next);
  };

  const selectSubtitle = (nextIndex: number) => {
    const option = subtitles[nextIndex];
    setSubtitleIndex(option ? nextIndex : -1);
    if (native) {
      void setNativeSubtitle(option?.source === "native" ? option.id : null)
        .catch((reason: unknown) => setError(errorMessage(reason)));
      return;
    }
    if (option?.source !== "sidecar") {
      setSubtitleCueUrl(undefined);
      return;
    }
    void subtitleCues(video.id, option.track)
      .then((cues) => setSubtitleCueUrl(`data:text/vtt;charset=utf-8,${encodeURIComponent(cues)}`))
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
      for (let position = 0; position < media.textTracks.length; position += 1) {
        const textTrack = media.textTracks[position];
        if (textTrack === own || (textTrack.kind !== "subtitles" && textTrack.kind !== "captions")) continue;
        found.push({
          source: "embedded",
          label: textTrack.label || textTrack.language.toUpperCase() || `Track ${position + 1}`,
          textTrack,
        });
      }
      setEmbeddedSubtitles((current) =>
        current.length === found.length && current.every((option, at) => option.label === found[at].label)
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
          ? textTrack === own
          : selectedSubtitle?.source === "embedded" && selectedSubtitle.textTrack === textTrack;
      textTrack.mode = showing ? "showing" : "disabled";
    }
  }, [native, selectedSubtitle, subtitleCueUrl]);

  useEffect(() => {
    const updateFullscreen = () => setFullscreen(document.fullscreenElement === playerShell.current);
    document.addEventListener("fullscreenchange", updateFullscreen);
    return () => document.removeEventListener("fullscreenchange", updateFullscreen);
  }, []);

  // Fullscreen is for watching, so the overlay gets out of the way until the
  // viewer reaches for it. Windowed playback always shows the controls.
  useEffect(() => {
    if (!fullscreen) {
      setControlsIdle(false);
      return;
    }
    let lastActivity = Date.now();
    const wake = () => {
      lastActivity = Date.now();
      setControlsIdle(false);
    };
    wake();
    // Polling rather than a one-shot timer so that moving the pointer off the
    // controls re-arms the countdown without needing its own listener.
    // Keyboard use keeps them up through the keydown listener below; focus is
    // deliberately not consulted, because clicking a control focuses it and
    // would then pin the overlay open for the rest of the session.
    const tick = window.setInterval(() => {
      if (pointerOverControls.current) lastActivity = Date.now();
      else if (Date.now() - lastActivity >= CONTROLS_IDLE_DELAY) setControlsIdle(true);
    }, 250);
    window.addEventListener("mousemove", wake);
    window.addEventListener("keydown", wake);
    return () => {
      window.clearInterval(tick);
      window.removeEventListener("mousemove", wake);
      window.removeEventListener("keydown", wake);
    };
  }, [fullscreen]);

  const toggleFullscreen = () => {
    if (document.fullscreenElement) {
      const exiting = document.exitFullscreen?.();
      if (exiting) void exiting.catch((reason: unknown) => setError(errorMessage(reason)));
      return;
    }
    const shell = playerShell.current;
    if (!shell?.requestFullscreen) {
      setError("Fullscreen mode is not supported by this system.");
      return;
    }
    void shell.requestFullscreen().catch((reason: unknown) => setError(errorMessage(reason)));
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey ||
        (target instanceof Element && target.closest("input, textarea, select, button, [contenteditable], [role=textbox]"))
      ) return;

      const run = (action: () => void) => {
        event.preventDefault();
        action();
      };
      if (event.key === " " || event.key === "Spacebar") {
        run(playingBack ? pause : play);
      } else if (event.key === "[") {
        run(() => rotate(-90));
      } else if (event.key === "]") {
        run(() => rotate(90));
      } else if (event.key === ",") {
        run(() => skip(-10));
      } else if (event.key === ".") {
        run(() => skip(10));
      } else if (event.shiftKey && event.key === "ArrowLeft" && index > 0) {
        run(() => selectVideo(index - 1));
      } else if (event.shiftKey && event.key === "ArrowRight" && index < videos.length - 1) {
        run(() => selectVideo(index + 1));
      } else if (event.key === "-") {
        run(() => stepSpeed(-1));
      } else if (event.key === "=" || event.key === "+") {
        run(() => stepSpeed(1));
      } else if (event.key.toLowerCase() === "s" && subtitles.length > 0) {
        run(toggleSubtitles);
      } else if (event.key.toLowerCase() === "l") {
        run(() => setLoop((enabled) => !enabled));
      } else if (event.key.toLowerCase() === "p" && videos.length > 1) {
        run(() => setPlaylistOpen((open) => !open));
      } else if (event.key.toLowerCase() === "f") {
        run(toggleFullscreen);
      } else if (event.key === "Escape") {
        run(fullscreen ? toggleFullscreen : onBack);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [currentTime, duration, fullscreen, index, native, nativeBaseRotation, onBack, playingBack, speed, subtitleIndex, subtitles, videos.length]);

  if (error) {
    const unsupported = error.includes("format") || error.includes("codec");
    return (
      <section className="player-error-state" aria-label={`Unable to play ${video.fileName}`}>
        <div className="unsupported-icon" aria-hidden="true"><span /></div>
        <h1>{unsupported ? "This video format isn't supported on your computer" : "This video could not be played"}</h1>
        <p>{video.fileName}</p>
        <p role="alert" className="sr-only">{error}</p>
        <div className="error-actions">
          <ControlButton shortcut="Escape" className="back-button" onClick={onBack} aria-label="Back to results">← Back to results</ControlButton>
          {index < videos.length - 1 ? <button type="button" className="playlist-button" onClick={() => setIndex((current) => current + 1)}>Skip to next</button> : null}
        </div>
      </section>
    );
  }

  return (
    <section className="player-view" aria-label={`Player for ${video.fileName}`}>
      <div className="player-heading">
        <ControlButton shortcut="Escape" className="back-button" onClick={onBack} aria-label="Back to results">
          ← Back
        </ControlButton>
        <h1 title={video.fileName}>{video.fileName}</h1>
        {videos.length > 1 ? (
          <ControlButton
            shortcut="P"
            className="playlist-toggle"
            aria-expanded={playlistOpen}
            onClick={() => setPlaylistOpen((open) => !open)}
          >
            Playlist <span className="playlist-count">{videos.length}</span>
          </ControlButton>
        ) : null}
      </div>

      {error ? <p role="alert" className="message error">{error}</p> : null}
      {!prepared && !error ? <p className="message">Preparing video…</p> : null}
      {prepared ? (
        <div ref={playerShell} className={controlsIdle ? "player-shell idle" : "player-shell"}>
          {native ? (
            <div ref={nativeSurface} className="native-video" aria-label={`Playing ${video.fileName}`} />
          ) : (
            <video
              ref={element}
              src={playbackSource(prepared.filePath)}
              aria-label={`Playing ${video.fileName}`}
              style={{ transform: `rotate(${rotation}deg)` }}
              onLoadedMetadata={(event) => {
                setDuration(Number.isFinite(event.currentTarget.duration) ? event.currentTarget.duration : 0);
                event.currentTarget.playbackRate = speed;
                play();
              }}
              onPlay={() => setPlayingBack(true)}
              onPause={() => setPlayingBack(false)}
              onTimeUpdate={(event) => setCurrentTime(event.currentTarget.currentTime)}
              onEnded={() => {
                if (index < videos.length - 1) setIndex((current) => current + 1);
                else if (loop && videos.length > 1) setIndex(0);
                else if (loop && element.current) {
                  element.current.currentTime = 0;
                  play();
                }
              }}
              onError={() => setError("This video format or codec is not supported on this computer.")}
            >
              {subtitleCueUrl && selectedSubtitle?.source === "sidecar" ? (
                <track
                  kind="subtitles"
                  src={subtitleCueUrl}
                  label={selectedSubtitle.label}
                  srcLang={selectedSubtitle.language ?? undefined}
                  default
                />
              ) : null}
            </video>
          )}
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
              type="range"
              min="0"
              max={duration || 0}
              step="0.1"
              value={Math.min(currentTime, duration || 0)}
              onChange={(event) => {
                const nextTime = Number(event.currentTarget.value);
                if (native) void seekNativeVideo(nextTime).catch((reason: unknown) => setError(errorMessage(reason)));
                else if (element.current) element.current.currentTime = nextTime;
                setCurrentTime(nextTime);
              }}
            />
            <div className="player-transport">
              <ControlButton shortcut="Shift+ArrowLeft" disabled={index === 0} onClick={() => selectVideo(index - 1)} aria-label="Previous video">⏮</ControlButton>
              <ControlButton shortcut="," onClick={() => skip(-10)} aria-label="Skip back 10 seconds">−10</ControlButton>
              <ControlButton shortcut="Space" className="play-button" onClick={play} aria-label="Play">
                <span className="play-glyph" aria-hidden="true" />
              </ControlButton>
              <ControlButton shortcut="Space" onClick={pause} aria-label="Pause">
                <span className="pause-glyph" aria-hidden="true" />
              </ControlButton>
              <ControlButton shortcut="." onClick={() => skip(10)} aria-label="Skip forward 10 seconds">+10</ControlButton>
              <ControlButton shortcut="Shift+ArrowRight" disabled={index === videos.length - 1} onClick={() => selectVideo(index + 1)} aria-label="Next video">⏭</ControlButton>
              <span className="time-display">{formatTime(currentTime)} / {formatTime(duration)}</span>
              <div className="player-utilities">
                <ControlButton
                  shortcut="S"
                  onClick={toggleSubtitles}
                  disabled={subtitles.length === 0}
                  aria-label="Subtitles"
                  aria-pressed={subtitleIndex >= 0}
                >
                  CC
                </ControlButton>
                {subtitles.length > 1 ? (
                  <select
                    aria-label="Subtitle track"
                    value={subtitleIndex}
                    onChange={(event) => selectSubtitle(Number(event.currentTarget.value))}
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
                    value={speed}
                    onChange={(event) => applySpeed(Number(event.currentTarget.value))}
                  >
                    {SPEEDS.map((value) => <option key={value} value={value}>{value}×</option>)}
                  </select>
                  <KeyHint shortcut="- =" />
                </span>
                <ControlButton shortcut="[" onClick={() => rotate(-90)} aria-label="Rotate left">↶</ControlButton>
                <ControlButton shortcut="]" onClick={() => rotate(90)} aria-label="Rotate right">↷</ControlButton>
                <ControlButton
                  shortcut="L"
                  onClick={() => setLoop((enabled) => !enabled)}
                  aria-label={videos.length > 1 ? "Loop playlist" : "Loop video"}
                  aria-pressed={loop}
                >
                  ⟳
                </ControlButton>
                <ControlButton shortcut="F" onClick={toggleFullscreen} aria-label={fullscreen ? "Exit fullscreen" : "Enter fullscreen"}>
                  {fullscreen ? "⤡" : "⛶"}
                </ControlButton>
              </div>
            </div>
          </div>
          {videos.length > 1 && playlistOpen ? (
            <aside className="playlist-drawer" aria-label="Playlist">
              <h2>Up next</h2>
              <ol>
                {videos.map((item, itemIndex) => (
                  <li key={item.id}>
                    <button
                      type="button"
                      className={itemIndex === index ? "active" : undefined}
                      aria-current={itemIndex === index ? "true" : undefined}
                      onClick={() => selectVideo(itemIndex)}
                      title={item.fileName}
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
      ) : null}
      {videos.length > 1 ? (
        <p className="playlist-status" aria-live="polite">
          Playlist video {index + 1} of {videos.length}
        </p>
      ) : null}
    </section>
  );
}

export default function App() {
  const [query, setQuery] = useState("");
  const [page, setPage] = useState<SearchPage>();
  const [playing, setPlaying] = useState<VideoResult[]>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const requestNumber = useRef(0);

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

  const submit = (event: FormEvent) => {
    event.preventDefault();
    void runSearch(query, 1);
  };

  const hasSubmitted = loading || Boolean(page) || Boolean(error) || Boolean(playing);

  return (
    <main className={hasSubmitted ? "app" : "app initial"}>
      <form role="search" onSubmit={submit} className="search-form">
        <label className="sr-only" htmlFor="video-search">Search videos</label>
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
          {query ? <button type="button" className="clear-search" aria-label="Clear search" onClick={() => setQuery("")}>×</button> : <span className="search-glyph" aria-hidden="true">⌕</span>}
        </div>
      </form>

      {loading ? <p className="message" aria-live="polite">Searching…</p> : null}
      {error ? <p role="alert" className="message error">{error}</p> : null}
      {playing && page ? <Player videos={playing} onBack={() => setPlaying(undefined)} /> : null}

      {!loading && !playing && page ? (
        <section className="results">
          <div className="results-summary">
            <p>{page.totalResults} {page.totalResults === 1 ? "video" : "videos"}</p>
            {page.results.length > 1 ? (
              <button type="button" className="playlist-button" onClick={() => setPlaying(page.results)}>
                Play all
              </button>
            ) : null}
            {page.totalPages > 0 ? <p>Page {page.page} of {page.totalPages}</p> : null}
          </div>
          {page.results.length ? (
            <ul className="video-grid" aria-label="Video results">
              {page.results.map((video) => (
                <li key={video.id}>
                  <button
                    type="button"
                    className="video-tile"
                    aria-label={`Play ${video.fileName}`}
                    title={video.fileName}
                    onClick={() => setPlaying([video])}
                  >
                    <span className="video-art"><VideoIcon /></span>
                    <span className="video-name">{video.fileName}</span>
                  </button>
                </li>
              ))}
            </ul>
          ) : (
            <p className="message">No matching videos found.</p>
          )}
          {page.totalPages > 1 ? (
            <nav className="pagination" aria-label="Search result pages">
              <button
                type="button"
                disabled={page.page <= 1}
                aria-label="Previous page"
                onClick={() => void runSearch(page.query, page.page - 1)}
              >
                Previous
              </button>
              <span>Page {page.page} of {page.totalPages}</span>
              <button
                type="button"
                disabled={page.page >= page.totalPages}
                aria-label="Next page"
                onClick={() => void runSearch(page.query, page.page + 1)}
              >
                Next
              </button>
            </nav>
          ) : null}
        </section>
      ) : null}
    </main>
  );
}
