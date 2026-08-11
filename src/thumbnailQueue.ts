import { videoThumbnail } from "./api";

export type ThumbnailLoader = (resultId: string) => Promise<string>;

type ThumbnailJob = {
  resultId: string;
  resolve: (path: string) => void;
  reject: (reason: unknown) => void;
};

export const THUMBNAIL_CONCURRENCY = 2;

export class ThumbnailQueue {
  private readonly queued: ThumbnailJob[] = [];
  private readonly pending = new Map<string, Promise<string>>();
  private readonly completed = new Map<string, string>();
  private active = 0;

  constructor(
    private readonly loadThumbnail: ThumbnailLoader,
    private readonly concurrency = THUMBNAIL_CONCURRENCY,
  ) {}

  load(resultId: string): Promise<string> {
    const completed = this.completed.get(resultId);
    if (completed) return Promise.resolve(completed);

    const pending = this.pending.get(resultId);
    if (pending) return pending;

    const promise = new Promise<string>((resolve, reject) => {
      this.queued.push({ resultId, resolve, reject });
    });
    this.pending.set(resultId, promise);
    this.pump();
    return promise;
  }

  private pump() {
    while (this.active < this.concurrency && this.queued.length > 0) {
      const job = this.queued.shift();
      if (!job) return;
      this.active += 1;

      void this.loadThumbnail(job.resultId)
        .then((path) => {
          this.completed.set(job.resultId, path);
          job.resolve(path);
        })
        .catch(job.reject)
        .finally(() => {
          this.active -= 1;
          this.pending.delete(job.resultId);
          this.pump();
        });
    }
  }
}

export const thumbnailQueue = new ThumbnailQueue(videoThumbnail);
