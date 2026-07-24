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

// jsdom doesn't implement the Blob URL registry.
let objectUrls = 0;
URL.createObjectURL = vi.fn(() => `blob:mock-${(objectUrls += 1)}`);
URL.revokeObjectURL = vi.fn();
