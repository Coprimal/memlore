/**
 * Save before lock: screens that hold unsaved edits register a flusher, and
 * every lock (manual, or native's `onFlushRequest` before an auto-lock) runs
 * `flushAll` first. A flusher saves what it holds and resolves once the
 * engine has stored it.
 */
export type Flusher = () => Promise<void>;

const flushers = new Set<Flusher>();

/** Registers `fn` until the returned function is called. */
export function registerFlusher(fn: Flusher): () => void {
  flushers.add(fn);
  return () => {
    flushers.delete(fn);
  };
}

/**
 * Runs every registered flusher and resolves once they all settled or after
 * `ms`, whichever comes first; at once when none is registered. Never
 * rejects: a failed or stalled save must never keep the vault unlocked.
 */
export function flushAll(ms: number): Promise<void> {
  if (flushers.size === 0) return Promise.resolve();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<void>((resolve) => {
    timer = setTimeout(resolve, ms);
  });
  // `then(fn)` turns a flusher that throws synchronously into a rejection.
  const all = Promise.allSettled([...flushers].map((fn) => Promise.resolve().then(fn)));
  return Promise.race([all.then(() => {}), timeout]).finally(() => clearTimeout(timer));
}
