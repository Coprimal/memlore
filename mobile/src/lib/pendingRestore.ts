/** A restore the user confirmed in version history, for the entry screen to run. */
export type PendingRestore = { entryId: string; versionId: string };

/** A confirmed restore not taken within this long is dropped (e.g. history was deep-linked). */
const PENDING_RESTORE_TTL_MS = 60_000;

// The history screen leaves it, then pops back to the entry screen, which takes
// it on focus (the entry screen restores: it holds the open editor). Every lock
// clears it, so a restore never outlives the unlock it was confirmed in.
let pendingRestore: PendingRestore | null = null;
let pendingSince = 0;

/** Leaves a confirmed restore for the entry screen; replaces any earlier one. */
export function setPendingRestore(restore: PendingRestore): void {
  pendingRestore = restore;
  pendingSince = Date.now();
}

/**
 * The pending restore for `entryId`, or null. Always empties the mailbox: one
 * left for another entry, or older than a minute, is abandoned, never run later.
 */
export function takePendingRestore(entryId: string): PendingRestore | null {
  const restore = pendingRestore;
  pendingRestore = null;
  if (Date.now() - pendingSince > PENDING_RESTORE_TTL_MS) return null;
  return restore?.entryId === entryId ? restore : null;
}

/** Drops any pending restore (on lock). */
export function clearPendingRestore(): void {
  pendingRestore = null;
}
