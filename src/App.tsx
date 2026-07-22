import { FormEvent, useEffect, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  prepareVideo,
  loadNativeVideo,
  nativePlaybackState,
  searchVideos,
  seekNativeVideo,
  setNativePaused,
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

function Player({ video, onBack }: { video: VideoResult; onBack: () => void }) {
  const element = useRef<HTMLVideoElement>(null);
  const nativeSurface = useRef<HTMLDivElement>(null);
  const [prepared, setPrepared] = useState<PreparedVideo>();
  const [duration, setDuration] = useState(0);
  const [currentTime, setCurrentTime] = useState(0);
  const [error, setError] = useState<string>();

  useEffect(() => {
    let active = true;
    let nativeActive = false;
    prepareVideo(video.id)
      .then((result) => {
        if (!active) return;
        setPrepared(result);
        if (result.playbackBackend === "native") {
          nativeActive = true;
          return loadNativeVideo(result.filePath);
        }
      })
      .catch((reason: unknown) => {
        if (active) setError(errorMessage(reason));
      });
    return () => {
      active = false;
      if (nativeActive) {
        void Promise.resolve(setNativeVideoBounds({ x: 0, y: 0, width: 1, height: 1, visible: false })).catch(() => {});
        void Promise.resolve(stopNativeVideo()).catch(() => {});
      }
    };
  }, [video.id]);

  const native = prepared?.playbackBackend === "native";

  useEffect(() => {
    if (!native || !nativeSurface.current) return;
    const surface = nativeSurface.current;
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
        })
        .catch((reason: unknown) => setError(errorMessage(reason)));
    }, 250);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", updateBounds);
      window.clearInterval(poll);
    };
  }, [native]);

  const play = () => {
    if (native) {
      void setNativePaused(false).catch((reason: unknown) => setError(errorMessage(reason)));
      return;
    }
    element.current?.play().catch(() => {
      setError("This video could not be played by the system media engine.");
    });
  };

  return (
    <section className="player-view" aria-label={`Player for ${video.fileName}`}>
      <div className="player-heading">
        <button type="button" className="back-button" onClick={onBack} aria-label="Back to results">
          ← Back
        </button>
        <h1 title={video.fileName}>{video.fileName}</h1>
      </div>

      {error ? <p role="alert" className="message error">{error}</p> : null}
      {!prepared && !error ? <p className="message">Preparing video…</p> : null}
      {prepared ? (
        <div className="player-shell">
          {native ? (
            <div ref={nativeSurface} className="native-video" aria-label={`Playing ${video.fileName}`} />
          ) : (
            <video
              ref={element}
              src={convertFileSrc(prepared.filePath)}
              aria-label={`Playing ${video.fileName}`}
              onLoadedMetadata={(event) => {
                setDuration(Number.isFinite(event.currentTarget.duration) ? event.currentTarget.duration : 0);
                play();
              }}
              onTimeUpdate={(event) => setCurrentTime(event.currentTarget.currentTime)}
              onError={() => setError("This video format or codec is not supported on this computer.")}
            />
          )}
          <div className="player-controls" aria-label="Video controls">
            <button type="button" onClick={play}>Play</button>
            <button type="button" onClick={() => {
              if (native) void setNativePaused(true).catch((reason: unknown) => setError(errorMessage(reason)));
              else element.current?.pause();
            }}>Pause</button>
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
          </div>
        </div>
      ) : null}
    </section>
  );
}

export default function App() {
  const [query, setQuery] = useState("");
  const [page, setPage] = useState<SearchPage>();
  const [selected, setSelected] = useState<VideoResult>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const requestNumber = useRef(0);

  const runSearch = async (submittedQuery: string, requestedPage: number) => {
    const trimmed = submittedQuery.trim();
    if (!trimmed) return;
    const currentRequest = ++requestNumber.current;
    setLoading(true);
    setError(undefined);
    setSelected(undefined);
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

  const hasSubmitted = loading || Boolean(page) || Boolean(error) || Boolean(selected);

  return (
    <main className={hasSubmitted ? "app" : "app initial"}>
      <form role="search" onSubmit={submit} className="search-form">
        <label className="sr-only" htmlFor="video-search">Search videos</label>
        <input
          id="video-search"
          type="search"
          value={query}
          onChange={(event) => setQuery(event.currentTarget.value)}
          placeholder="Search videos…"
          autoComplete="off"
          autoFocus
        />
      </form>

      {loading ? <p className="message" aria-live="polite">Searching…</p> : null}
      {error ? <p role="alert" className="message error">{error}</p> : null}
      {selected && page ? <Player video={selected} onBack={() => setSelected(undefined)} /> : null}

      {!loading && !selected && page ? (
        <section className="results">
          <div className="results-summary">
            <p>{page.totalResults} {page.totalResults === 1 ? "video" : "videos"}</p>
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
                    onClick={() => setSelected(video)}
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
