/**
 * Days an entry stays in Trash before the sweep deletes it for good. Mirrors
 * the desktop's `TRASH_RETENTION_DAYS` (`src/hooks/useTrash.ts`) and the
 * engine's `TRASH_RETENTION_SECS`.
 */
export const TRASH_RETENTION_DAYS = 30;

const DAY_SECS = 86_400;

/**
 * Days left before the sweep purges an entry trashed at `trashedAt` (unix
 * seconds), as on desktop: whole days of age, never negative when a peer's
 * clock is ahead of ours, and never below 0 when the sweep lags.
 */
export function trashDaysLeft(trashedAt: number, nowSecs: number): number {
  const age = Math.max(0, Math.floor((nowSecs - trashedAt) / DAY_SECS));
  return Math.max(0, TRASH_RETENTION_DAYS - age);
}
