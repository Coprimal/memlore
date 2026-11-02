import { useFocusEffect } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';

import MemloreCore, { type Version } from '@/modules/memlore-core';
import { errorCode, useVault } from '@/src/hooks/useVault';

export {
  type PendingRestore,
  setPendingRestore,
  takePendingRestore,
} from '@/src/lib/pendingRestore';

/**
 * An entry's saved versions, newest first, read on every focus of the screen
 * (and by `refresh`); an older answer is dropped. `loading` is true until the
 * first answer only, so a re-read never blanks the list or its empty state.
 * `error` is a failed read;
 * ERR_LOCKED shows the lock screen instead; ERR_NOT_FOUND sets `notFound` (the
 * entry is gone: the screen goes back, as the entry screen does).
 * `content(versionId)` resolves the version's Yjs document (base64), or null
 * when the read failed; a version gone meanwhile reloads the list. Components
 * use this, never the native module.
 */
export function useVersions(entryId: string) {
  const vault = useVault();
  const [versions, setVersions] = useState<Version[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  const [notFound, setNotFound] = useState(false);
  // Bumped by each read and on unmount: an older answer is dropped.
  const generation = useRef(0);

  useEffect(() => {
    return () => {
      generation.current += 1;
    };
  }, []);

  const refresh = useCallback(async () => {
    const gen = ++generation.current;
    try {
      const next = await MemloreCore.listEntryVersions(entryId);
      if (gen !== generation.current) return;
      setVersions(next);
      setError(false);
      setNotFound(false);
    } catch (e) {
      if (gen !== generation.current) return;
      const code = errorCode(e);
      if (code === 'ERR_LOCKED') vault.markLocked();
      else if (code === 'ERR_NOT_FOUND') setNotFound(true);
      else setError(true);
    } finally {
      if (gen === generation.current) setLoading(false);
    }
  }, [entryId]);

  useFocusEffect(
    useCallback(() => {
      void refresh();
    }, [refresh]),
  );

  const content = useCallback(
    async (versionId: string): Promise<string | null> => {
      try {
        return await MemloreCore.getEntryVersionContent(entryId, versionId);
      } catch (e) {
        const code = errorCode(e);
        if (code === 'ERR_LOCKED') vault.markLocked();
        // The version was pruned, or the entry is gone (the reload says which).
        else if (code === 'ERR_NOT_FOUND') void refresh();
        return null;
      }
    },
    [entryId, refresh],
  );

  return { versions, loading, error, notFound, refresh, content };
}
