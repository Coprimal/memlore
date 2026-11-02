/** A save after more than this long since the last save starts a new session (desktop). */
export const SESSION_IDLE_MS = 10 * 60 * 1000;

export interface VersionSession {
  /**
   * Whether this save should snapshot the stored doc first. Sets the guard at
   * once, so an overlapping save in the same session skips the snapshot.
   */
  shouldSnapshot(nowMs: number): boolean;
  /** `false` (blank stored text, nothing kept) reopens the session. */
  result(snapshotted: boolean): void;
  /** Records a successful save; the idle gap is measured between saves. */
  noteSave(nowMs: number): void;
  /**
   * The current content was just kept (pre-restore snapshot): skip the next
   * one. Counts as a save for the idle gap, so the restore's own save after a
   * long idle does not keep the same content again.
   */
  markTaken(nowMs: number): void;
}

/**
 * The version-history session of one open entry: at most one snapshot per
 * session, taken by the first content save. Before any save (or `markTaken`)
 * is noted there is no idle gap.
 */
export function createVersionSession(): VersionSession {
  let taken = false;
  let lastSave: number | null = null;
  return {
    shouldSnapshot(nowMs) {
      if (taken && lastSave !== null && nowMs - lastSave > SESSION_IDLE_MS) taken = false;
      if (taken) return false;
      taken = true;
      return true;
    },
    result(snapshotted) {
      if (!snapshotted) taken = false;
    },
    noteSave(nowMs) {
      lastSave = nowMs;
    },
    markTaken(nowMs) {
      taken = true;
      lastSave = nowMs;
    },
  };
}
