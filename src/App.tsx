import { FormEvent, useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  loadNativeVideo,
  nativePlaybackState,
  nativeVideoRotation,
  prepareVideo,
  searchVideos,
  seekNativeVideo,
  setNativePaused,
  setNativeVideoRotation,
  setNativeVideoBounds,
  stopNativeVideo,
  type PreparedVideo,
  type SearchPage,
  type VideoResult,
} from "./api";

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

function formatTime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  const wholeSeconds = Math.floor(seconds);
  return `${Math.floor(wholeSeconds / 60)}:${String(wholeSeconds % 60).padStart(2, "0")}`;
}

function Player({ videos, onBack }: { videos: VideoResult[]; onBack: () => void }) {
  const element = useRef<HTMLVideoElement>(null);
  const playerShell = useRef<HTMLDivElement>(null);
  const nativeSurface = useRef<HTMLDivElement>(null);
  const [index, setIndex] = useState(0);
  const [prepared, setPrepared] = useState<PreparedVideo>();
  const [duration, setDuration] = useState(0);
  const [currentTime, setCurrentTime] = useState(0);
  const [error, setError] = useState<string>();
  const [fullscreen, setFullscreen] = useState(false);
  const [loop, setLoop] = useState(false);
  const [playingBack, setPlayingBack] = useState(false);
  const [rotation, setRotation] = useState(0);
  const [nativeBaseRotation, setNativeBaseRotation] = useState(0);
  const [playlistOpen, setPlaylistOpen] = useState(videos.length > 1);
  const video = videos[index];

  useEffect(() => {
    let active = true;
    let nativeActive = false;
    setPrepared(undefined);
    setDuration(0);
    setCurrentTime(0);
    setError(undefined);
    setPlayingBack(false);
    setRotation(0);
    setNativeBaseRotation(0);
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

  useEffect(() => {
    if (!native || !nativeSurface.current) return;
    const surface = nativeSurface.current;
    let advancing = false;
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

  const selectVideo = (nextIndex: number) => {
    if (nextIndex >= 0 && nextIndex < videos.length) setIndex(nextIndex);
  };

  useEffect(() => {
    const updateFullscreen = () => setFullscreen(document.fullscreenElement === playerShell.current);
    document.addEventListener("fullscreenchange", updateFullscreen);
    return () => document.removeEventListener("fullscreenchange", updateFullscreen);
  }, []);

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
      } else if (event.shiftKey && event.key === "ArrowLeft" && index > 0) {
        run(() => selectVideo(index - 1));
      } else if (event.shiftKey && event.key === "ArrowRight" && index < videos.length - 1) {
        run(() => selectVideo(index + 1));
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
  }, [fullscreen, index, native, nativeBaseRotation, onBack, playingBack, videos.length]);

  return (
    <section className="player-view" aria-label={`Player for ${video.fileName}`}>
      <div className="player-heading">
        <button type="button" className="back-button" onClick={onBack} aria-label="Back to results" aria-keyshortcuts="Escape">
          ← Back
        </button>
        <h1 title={video.fileName}>{video.fileName}</h1>
        {videos.length > 1 ? (
          <button
            type="button"
            className="playlist-toggle"
            aria-expanded={playlistOpen}
            aria-keyshortcuts="P"
            onClick={() => setPlaylistOpen((open) => !open)}
          >
            Playlist <span>{videos.length}</span>
          </button>
        ) : null}
      </div>

      {error ? <p role="alert" className="message error">{error}</p> : null}
      {!prepared && !error ? <p className="message">Preparing video…</p> : null}
      {prepared ? (
        <div ref={playerShell} className="player-shell">
          {native ? (
            <div ref={nativeSurface} className="native-video" aria-label={`Playing ${video.fileName}`} />
          ) : (
            <video
              ref={element}
              src={convertFileSrc(prepared.filePath)}
              aria-label={`Playing ${video.fileName}`}
              style={{ transform: `rotate(${rotation}deg)` }}
              onLoadedMetadata={(event) => {
                setDuration(Number.isFinite(event.currentTarget.duration) ? event.currentTarget.duration : 0);
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
            />
          )}
          <div className="player-controls" aria-label="Video controls">
            <button type="button" className="transport-button" disabled={index === 0} onClick={() => selectVideo(index - 1)} aria-label="Previous video" aria-keyshortcuts="Shift+ArrowLeft">◀◀</button>
            <button type="button" className="transport-button" onClick={() => rotate(-90)} aria-label="Rotate left" aria-keyshortcuts="[">↶</button>
            <button type="button" className="play-button" onClick={play} aria-keyshortcuts="Space">Play</button>
            <button type="button" className="transport-button" onClick={pause} aria-keyshortcuts="Space">Pause</button>
            <button type="button" className="transport-button" onClick={() => rotate(90)} aria-label="Rotate right" aria-keyshortcuts="]">↷</button>
            <button type="button" className="transport-button" disabled={index === videos.length - 1} onClick={() => selectVideo(index + 1)} aria-label="Next video" aria-keyshortcuts="Shift+ArrowRight">▶▶</button>
            <button
              type="button"
              className="transport-button"
              onClick={() => setLoop((enabled) => !enabled)}
              aria-label={videos.length > 1 ? "Loop playlist" : "Loop video"}
              aria-pressed={loop}
              aria-keyshortcuts="L"
            >
              {loop ? "Looping" : "Loop"}
            </button>
            <button type="button" className="transport-button" onClick={toggleFullscreen} aria-label={fullscreen ? "Exit fullscreen" : "Enter fullscreen"} aria-keyshortcuts="F">
              {fullscreen ? "Exit fullscreen" : "Fullscreen"}
            </button>
            <input
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
            <span className="time-display">{formatTime(currentTime)} / {formatTime(duration)}</span>
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
