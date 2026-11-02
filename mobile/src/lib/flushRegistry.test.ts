import { afterEach, describe, expect, it, vi } from 'vitest';
import { flushAll, registerFlusher } from './flushRegistry';

// The lock-safety contract: a lock waits for the saves, but a stalled or
// failed save never holds the vault unlocked past the timeout.
describe('flushAll', () => {
  const unregister: (() => void)[] = [];
  afterEach(() => {
    unregister.splice(0).forEach((fn) => fn());
    vi.useRealTimers();
  });

  async function settledAfter(ms: number, promise: Promise<void>): Promise<boolean> {
    let settled = false;
    void promise.then(() => {
      settled = true;
    });
    await vi.advanceTimersByTimeAsync(ms);
    return settled;
  }

  it('resolves at once when nothing is registered', async () => {
    vi.useFakeTimers();
    expect(await settledAfter(0, flushAll(2000))).toBe(true);
  });

  it('waits for every flusher, and resolves even when one rejects', async () => {
    vi.useFakeTimers();
    const saved: string[] = [];
    unregister.push(
      registerFlusher(async () => {
        await new Promise((resolve) => setTimeout(resolve, 100));
        saved.push('title');
      }),
      registerFlusher(() => Promise.reject(new Error('save failed'))),
    );
    const done = flushAll(2000);
    expect(await settledAfter(50, done)).toBe(false);
    expect(await settledAfter(50, done)).toBe(true);
    expect(saved).toEqual(['title']);
  });

  it('resolves at the timeout when a flusher never settles', async () => {
    vi.useFakeTimers();
    unregister.push(registerFlusher(() => new Promise<void>(() => {})));
    const done = flushAll(1800);
    expect(await settledAfter(1799, done)).toBe(false);
    expect(await settledAfter(1, done)).toBe(true);
  });
});
