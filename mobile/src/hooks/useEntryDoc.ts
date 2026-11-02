import { useCallback, useEffect, useState } from 'react';

import MemloreCore from '@/modules/memlore-core';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { getCachedRow, putCachedRow } from '@/src/lib/rowCache';

export type EntryDocStatus = 'loading' | 'ready' | 'notFound' | 'failed';

/**
 * One entry's Yjs document for the DOM editor: the stored full state
 * (base64), the save the editor calls with its full state, and the title
 * save. The body save goes through the engine's merged save, like the
 * desktop editor. ERR_LOCKED shows the lock screen; ERR_NOT_FOUND (the entry
 * is gone, locked or invisible) sets `status` to 'notFound'. Components use
 * this, never the native module.
 */
export function useEntryDoc(id: string) {
  const vault = useVault();
  const [stateB64, setStateB64] = useState<string | null>(null);
  const [status, setStatus] = useState<EntryDocStatus>('loading');

  /** Maps a load or save error; anything but these two leaves `status` alone. */
  function onError(e: unknown) {
    const code = errorCode(e);
    if (code === 'ERR_LOCKED') vault.markLocked();
    else if (code === 'ERR_NOT_FOUND') setStatus('notFound');
  }

  useEffect(() => {
    let cancelled = false;
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
        setStatus('failed');
        onError(e);
      });
    return () => {
      cancelled = true;
    };
  }, [id]);

  /** Rejects when the save failed, so the editor retries on its next edit. */
  const save = useCallback(
    async (fullStateB64: string, contentText: string, previewText: string) => {
      try {
        await MemloreCore.saveEntryYjs(id, fullStateB64, contentText, previewText);
      } catch (e) {
        onError(e);
        throw e;
      }
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

  return { stateB64, status, save, saveTitle };
}
