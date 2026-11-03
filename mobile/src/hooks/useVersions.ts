import { useFocusEffect } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';

import MemloreCore, { type Version } from '@/modules/memlore-core';
import { useLockSession } from '@/src/hooks/useLockSession';
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
 * entry is gone: the screen goes back, as the entry screen does);
 * ERR_SECOND_LOCK_REQUIRED (a locked entry, not revealed) sets `lockRequired`.
 * A reveal or re-lock (the lock session's `generation`) reads again, and the
 * list read before it is never returned (fail closed: `loading` until the new
 * answer).
 * `content(versionId)` resolves the version's Yjs document (base64), or null
 * when the read failed; a version gone meanwhile reloads the list. Components
 * use this, never the native module.
 */
export function useVersions(entryId: string) {
  const vault = useVault();
  const { generation: lockGeneration } = useLockSession();
  // The same, for a read that must say which lock state it ran in.
  const lockGenerationRef = useRef(lockGeneration);
  lockGenerationRef.current = lockGeneration;
  const [versions, setVersions] = useState<Version[]>([]);
  // The lock generation `versions` were read in.
  const [readIn, setReadIn] = useState(lockGeneration);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  const [notFound, setNotFound] = useState(false);
  const [lockRequired, setLockRequired] = useState(false);
  // Bumped by each read and on unmount: an older answer is dropped.
  const generation = useRef(0);

  useEffect(() => {
    return () => {
      generation.current += 1;
    };
  }, []);

  const refresh = useCallback(async () => {
    const gen = ++generation.current;
    const lockGen = lockGenerationRef.current;
    try {
      const next = await MemloreCore.listEntryVersions(entryId);
      if (gen !== generation.current) return;
      setVersions(next);
      setReadIn(lockGen);
      setError(false);
      setNotFound(false);
      setLockRequired(false);
    } catch (e) {
      if (gen !== generation.current) return;
      const code = errorCode(e);
      if (code === 'ERR_LOCKED') vault.markLocked();
      else if (code === 'ERR_SECOND_LOCK_REQUIRED' || code === 'ERR_NOT_FOUND') {
        setVersions([]);
        // Empty is this generation's answer (no endless spinner).
        setReadIn(lockGen);
        if (code === 'ERR_NOT_FOUND') setNotFound(true);
        else setLockRequired(true);
      } else setError(true);
    } finally {
      if (gen === generation.current) setLoading(false);
    }
  }, [entryId]);

  useFocusEffect(
    useCallback(() => {
      void refresh();
    }, [refresh]),
  );

  // A reveal or re-lock: read again (a locked entry's read is now refused).
  const seenLockGeneration = useRef(lockGeneration);
  useEffect(() => {
    if (seenLockGeneration.current === lockGeneration) return;
    seenLockGeneration.current = lockGeneration;
    void refresh();
  }, [lockGeneration, refresh]);

  const content = useCallback(
    async (versionId: string): Promise<string | null> => {
      try {
        return await MemloreCore.getEntryVersionContent(entryId, versionId);
      } catch (e) {
        const code = errorCode(e);
        if (code === 'ERR_LOCKED') vault.markLocked();
        else if (code === 'ERR_SECOND_LOCK_REQUIRED') setLockRequired(true);
        // The version was pruned, or the entry is gone (the reload says which).
        else if (code === 'ERR_NOT_FOUND') void refresh();
        return null;
      }
    },
    [entryId, refresh],
  );

  // Read before the last reveal or re-lock: never shown.
  const stale = readIn !== lockGeneration;
  return {
    versions: stale ? NO_VERSIONS : versions,
    loading: loading || (stale && !error),
    error,
    notFound,
    lockRequired,
    refresh,
    content,
  };
}

const NO_VERSIONS: Version[] = [];
