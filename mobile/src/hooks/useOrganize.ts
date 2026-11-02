import { useFocusEffect } from 'expo-router';
import {
  type Dispatch,
  type SetStateAction,
  useCallback,
  useEffect,
  useRef,
  useState,
} from 'react';

import MemloreCore, { type Journal, type Tag, type TrashRow } from '@/modules/memlore-core';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { removeCachedRow } from '@/src/lib/rowCache';

export { TRASH_RETENTION_DAYS, trashDaysLeft } from '@/src/lib/trashRetention';

/**
 * A list read with `load` on every focus of the screen (and by `refresh`).
 * `error` is a failed read; ERR_LOCKED shows the lock screen instead.
 */
function useFocusList<T>(load: () => Promise<T[]>) {
  const vault = useVault();
  const [items, setItems] = useState<T[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  // Bumped by each read and on unmount: an older answer is dropped.
  const generation = useRef(0);

  useEffect(() => {
    return () => {
      generation.current += 1;
    };
  }, []);

  const refresh = useCallback(async () => {
    const gen = ++generation.current;
    setLoading(true);
    try {
      const next = await load();
      if (gen !== generation.current) return;
      setItems(next);
      setError(false);
    } catch (e) {
      if (gen !== generation.current) return;
      if (errorCode(e) === 'ERR_LOCKED') vault.markLocked();
      else setError(true);
    } finally {
      if (gen === generation.current) setLoading(false);
    }
  }, [load]);

  useFocusEffect(
    useCallback(() => {
      void refresh();
    }, [refresh]),
  );

  return { items, setItems, loading, error, refresh };
}

/**
 * Maps a refused change: ERR_LOCKED shows the lock screen; ERR_NOT_FOUND (the
 * row is gone), or any error when `reload`, reads the list again.
 */
function useChangeFailure(refresh: () => Promise<void>) {
  const vault = useVault();
  return (e: unknown, reload = false) => {
    const code = errorCode(e);
    if (code === 'ERR_LOCKED') vault.markLocked();
    else if (reload || code === 'ERR_NOT_FOUND') void refresh();
  };
}

function without<T extends { id: string }>(
  set: Dispatch<SetStateAction<T[]>>,
  ids: readonly string[],
) {
  const gone = new Set(ids);
  set((prev) => prev.filter((item) => !gone.has(item.id)));
}

const loadTrash = () => MemloreCore.listTrash();
const loadJournals = () => MemloreCore.listJournals();
const loadTags = () => MemloreCore.listTags();

/**
 * Trash, most recently trashed first (locked and invisible entries are never
 * listed), read on each focus. Restore and purge take the row off at once and
 * resolve true once done; on failure the list reloads (it is right again)
 * and they resolve false. Use `trashDaysLeft(row.trashedAt, nowSecs)` for
 * the days left. Components use this, never the native module.
 */
export function useTrash() {
  const { items: rows, setItems: setRows, loading, error, refresh } = useFocusList(loadTrash);
  const onFail = useChangeFailure(refresh);

  /** Back to its journal; the entry list shows it on its next focus. */
  const restore = useCallback(
    async (id: string) => {
      without(setRows, [id]);
      try {
        await MemloreCore.restoreEntry(id);
        return true;
      } catch (e) {
        onFail(e, true);
        return false;
      }
    },
    [refresh],
  );

  /** Deletes for good; ask for confirmation first. */
  const purge = useCallback(
    async (id: string) => {
      without(setRows, [id]);
      try {
        await MemloreCore.purgeEntry(id);
        removeCachedRow(id);
        return true;
      } catch (e) {
        onFail(e, true);
        return false;
      }
    },
    [refresh],
  );

  /**
   * Deletes for good the confirmed `ids` (pass the rows shown on screen when
   * the user confirmed); an id no longer in Trash is skipped. Resolves with
   * how many were deleted, or null when it failed (the list reloads).
   */
  const emptyTrash = useCallback(
    async (ids: readonly string[]): Promise<number | null> => {
      without(setRows, ids);
      try {
        const purged = await MemloreCore.emptyTrash([...ids]);
        for (const id of ids) removeCachedRow(id);
        return purged;
      } catch (e) {
        onFail(e, true);
        return null;
      }
    },
    [refresh],
  );

  return { rows, loading, error, refresh, restore, purge, emptyTrash };
}

/**
 * The listed journals (visible, unlocked), read on each focus. Create and
 * rename trim the name (empty resolves null without a call) and resolve the
 * stored journal, or null when refused (ERR_FORMAT, or ERR_NOT_FOUND for a
 * journal gone, which reloads the list). Components use this, never the
 * native module.
 */
export function useJournals() {
  const { items: journals, setItems, loading, error, refresh } = useFocusList(loadJournals);
  const onFail = useChangeFailure(refresh);

  const createJournal = useCallback(
    async (name: string): Promise<Journal | null> => {
      const trimmed = name.trim();
      if (!trimmed) return null;
      try {
        const journal = await MemloreCore.createJournal(trimmed);
        setItems((prev) => [...prev.filter((j) => j.id !== journal.id), journal]);
        return journal;
      } catch (e) {
        onFail(e);
        return null;
      }
    },
    [refresh],
  );

  const renameJournal = useCallback(
    async (id: string, name: string): Promise<Journal | null> => {
      const trimmed = name.trim();
      if (!trimmed) return null;
      try {
        const journal = await MemloreCore.renameJournal(id, trimmed);
        setItems((prev) => prev.map((j) => (j.id === journal.id ? journal : j)));
        return journal;
      } catch (e) {
        onFail(e);
        return null;
      }
    },
    [refresh],
  );

  return { journals, loading, error, refresh, createJournal, renameJournal };
}

/**
 * Every tag the picker may offer, sorted by name (tags used only on locked or
 * invisible entries are left out), read on each focus. Putting a tag on or
 * off the open entry is `useEntryActions(id).addTag/removeTag`, so its pills
 * follow at once. Components use this, never the native module.
 */
export function useTags() {
  const { items: tags, setItems, loading, error, refresh } = useFocusList(loadTags);
  const onFail = useChangeFailure(refresh);

  /**
   * Get-or-create: the tag named `name` (trimmed), created when missing; it
   * joins `tags` when not listed yet. Resolves null for an empty name or a
   * refused call.
   */
  const createTag = useCallback(
    async (name: string): Promise<Tag | null> => {
      const trimmed = name.trim();
      if (!trimmed) return null;
      try {
        const tag = await MemloreCore.createTag(trimmed);
        setItems((prev) => (prev.some((t) => t.id === tag.id) ? prev : [...prev, tag]));
        return tag;
      } catch (e) {
        onFail(e);
        return null;
      }
    },
    [refresh],
  );

  return { tags, loading, error, refresh, createTag };
}
