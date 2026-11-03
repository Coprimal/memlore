import { useCallback, useEffect, useRef, useState } from 'react';

import MemloreCore, { type MemloreErrorCode } from '@/modules/memlore-core';
import { noteActivity } from '@/src/hooks/useLockSession';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { getCachedRow, putCachedRow } from '@/src/lib/rowCache';
import { createVersionSession } from '@/src/lib/versionSession';

export type EntryDocStatus = 'loading' | 'ready' | 'notFound' | 'lockRequired' | 'failed';

/** A version's Yjs document (base64), or why it could not be read. */
export type VersionRead = { b64: string } | { error: MemloreErrorCode };

/**
 * One entry's Yjs document for the DOM editor: the stored full state
 * (base64), the save the editor calls with its full state, and the title
 * save. The body save goes through the engine's merged save, like the
 * desktop editor. ERR_LOCKED shows the lock screen; ERR_NOT_FOUND (the entry
 * is gone, locked or invisible) sets `status` to 'notFound'; a load refused
 * with ERR_SECOND_LOCK_REQUIRED sets 'lockRequired' (reveal, then bump
 * `attempt` to load again). Components use this, never the native module.
 *
 * Version history, like the desktop: the first content save of a session
 * keeps the STORED doc as a version before it saves. A session is one load of
 * the entry, or a save after more than 10 min since the last one.
 * `markSnapshotTaken` skips the next snapshot (the pre-restore one was taken);
 * `snapshot` and `versionContent` are the restore's two reads, and
 * `lastSaveError` says whether the body on screen is stored (null) first.
 */
export function useEntryDoc(id: string, attempt = 0) {
  const vault = useVault();
  const [stateB64, setStateB64] = useState<string | null>(null);
  const [status, setStatus] = useState<EntryDocStatus>('loading');
  const sessionRef = useRef(createVersionSession());
  // The outcome of the last body save started: null once it succeeded, its
  // error code (ERR_IO when unknown) when it failed. An older save's outcome
  // is ignored: each save sends the full state, so the newest one decides.
  const saveError = useRef<MemloreErrorCode | null>(null);
  const saveSeq = useRef(0);

  /** Maps a load or save error; anything but these two leaves `status` alone. */
  function onError(e: unknown) {
    const code = errorCode(e);
    if (code === 'ERR_LOCKED') vault.markLocked();
    else if (code === 'ERR_NOT_FOUND') setStatus('notFound');
  }

  useEffect(() => {
    let cancelled = false;
    sessionRef.current = createVersionSession();
    saveError.current = null;
    setStatus('loading');
    setStateB64(null);
    MemloreCore.loadEntryYjs(id)
      .then((b64) => {
        if (cancelled) return;
        setStateB64(b64);
        setStatus('ready');
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        // Only the load asks for the reveal: a save refused by a re-lock
        // leaves `status` alone (the screen leaves on the re-lock).
        setStatus(errorCode(e) === 'ERR_SECOND_LOCK_REQUIRED' ? 'lockRequired' : 'failed');
        onError(e);
      });
    return () => {
      cancelled = true;
    };
  }, [id, attempt]);

  /** Rejects when the save failed, so the editor retries on its next edit. */
  const save = useCallback(
    async (fullStateB64: string, contentText: string, previewText: string) => {
      // Typing is activity for the second lock's idle re-lock: keys are not
      // touches, and the editor's WebView may hide its touches from the root.
      noteActivity();
      const session = sessionRef.current;
      const now = Date.now();
      const seq = ++saveSeq.current;
      const settle = (code: MemloreErrorCode | null) => {
        if (seq === saveSeq.current) saveError.current = code;
      };
      // The snapshot keeps the stored doc as it was before this save:
      // - it is awaited before saveEntryYjs in this same handler;
      // - the Swift engineQueue is serial, so it runs before that save;
      // - an overlapping second save sees the guard already set and skips it;
      // - a reordered older full-state merge is a CRDT no-op;
      // - until M2 nothing else writes the stored doc between saves.
      if (session.shouldSnapshot(now)) {
        try {
          const versionId = await MemloreCore.snapshotEntryVersion(id);
          // Blank stored text: nothing kept, the next save tries again.
          if (versionId === null) session.result(false);
        } catch (e) {
          // Best effort: a missed snapshot is one fewer checkpoint. Other
          // errors (ERR_NOT_FOUND) come back from the save below.
          if (errorCode(e) === 'ERR_LOCKED') {
            session.result(false);
            settle('ERR_LOCKED');
            onError(e);
            throw e;
          }
        }
      }
      try {
        await MemloreCore.saveEntryYjs(id, fullStateB64, contentText, previewText);
      } catch (e) {
        settle(errorCode(e) ?? 'ERR_IO');
        onError(e);
        throw e;
      }
      settle(null);
      session.noteSave(now);
    },
    [id],
  );

  /**
   * Saves the title as typed ('' clears it) and updates the list's cached row,
   * so the list shows it on return. Rejects when the save failed.
   */
  const saveTitle = useCallback(async (entryId: string, title: string) => {
    try {
      await MemloreCore.setEntryTitle(entryId, title);
    } catch (e) {
      onError(e);
      throw e;
    }
    const row = getCachedRow(entryId);
    if (row) putCachedRow({ ...row, title: title === '' ? null : title });
  }, []);

  /**
   * Drops the document read (the entry's other read was refused by a re-lock
   * meanwhile): the editor unmounts until `attempt` loads it again.
   */
  const discard = useCallback(() => {
    setStateB64(null);
    setStatus('loading');
  }, []);

  const markSnapshotTaken = useCallback(() => sessionRef.current.markTaken(Date.now()), []);

  /** Why the last body save failed, or null when it succeeded (or none ran). */
  const lastSaveError = useCallback(() => saveError.current, []);

  /**
   * Keeps the stored doc as a version now, outside the session (the
   * pre-restore snapshot, taken on every restore like the desktop). Resolves
   * the version id, or null when the stored text is blank (nothing kept).
   * Rejects when it failed.
   */
  const snapshot = useCallback(async (): Promise<string | null> => {
    try {
      return await MemloreCore.snapshotEntryVersion(id);
    } catch (e) {
      onError(e);
      throw e;
    }
  }, [id]);

  /** A version's Yjs document (base64), or the read's error code (ERR_IO when unknown). */
  const versionContent = useCallback(
    async (versionId: string): Promise<VersionRead> => {
      try {
        return { b64: await MemloreCore.getEntryVersionContent(id, versionId) };
      } catch (e) {
        const code = errorCode(e) ?? 'ERR_IO';
        if (code === 'ERR_LOCKED') vault.markLocked();
        // ERR_NOT_FOUND: the version was pruned, or the entry is gone; the
        // entry's own reads say which.
        return { error: code };
      }
    },
    [id],
  );

  return {
    stateB64,
    status,
    save,
    saveTitle,
    discard,
    markSnapshotTaken,
    lastSaveError,
    snapshot,
    versionContent,
  };
}
