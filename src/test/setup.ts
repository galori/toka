import "@testing-library/jest-dom/vitest";

globalThis.ResizeObserver = class ResizeObserverMock {
  observe() {}
  unobserve() {}
  disconnect() {}
};

Object.defineProperty(HTMLMediaElement.prototype, "play", {
  configurable: true,
  value: vi.fn().mockResolvedValue(undefined),
});

Object.defineProperty(HTMLMediaElement.prototype, "pause", {
  configurable: true,
  value: vi.fn(),
});

class VTTCueMock {
  constructor(
    public startTime: number,
    public endTime: number,
    public text: string,
  ) {}
}

globalThis.VTTCue = VTTCueMock as unknown as typeof VTTCue;

type TextTrackMock = TextTrack & {
  cues: VTTCue[];
  activeCues: VTTCue[];
  addCue: (cue: VTTCue) => void;
};

type TextTrackListMock = TextTrack[] & {
  addEventListener: () => void;
  removeEventListener: () => void;
};

const textTracks = new WeakMap<HTMLMediaElement, TextTrackListMock>();

function trackListFor(media: HTMLMediaElement): TextTrackListMock {
  let list = textTracks.get(media);
  if (!list) {
    list = [] as unknown as TextTrackListMock;
    list.addEventListener = vi.fn();
    list.removeEventListener = vi.fn();
    textTracks.set(media, list);
  }
  return list;
}

Object.defineProperty(HTMLMediaElement.prototype, "textTracks", {
  configurable: true,
  get() {
    return trackListFor(this);
  },
});

Object.defineProperty(HTMLMediaElement.prototype, "addTextTrack", {
  configurable: true,
  value(kind: TextTrackKind, label = "", language = "") {
    const track = {
      kind,
      label,
      language,
      mode: "disabled",
      cues: [],
      activeCues: [],
      addCue(this: TextTrackMock, cue: VTTCue) {
        this.cues.push(cue);
      },
    } as unknown as TextTrackMock;
    trackListFor(this).push(track);
    return track;
  },
});
