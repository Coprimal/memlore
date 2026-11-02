import { useFocusEffect } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';

import MemloreCore, {
  type Emotion,
  type EntryDetail,
  type EntryPage,
  type EntryRow,
  type Tag,
} from '@/modules/memlore-core';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { flushAll } from '@/src/lib/flushRegistry';
import { getCachedRow, putCachedRow, removeCachedRow } from '@/src/lib/rowCache';

// The row cache lives in its own module so useVault can clear it on every lock
// without importing this file.
export { getCachedRow, putCachedRow, removeCachedRow } from '@/src/lib/rowCache';

/** `items` appended to `rows` without the ids already there (pages are newest first). */
function appendNew(rows: readonly EntryRow[], items: readonly EntryRow[]): EntryRow[] {
  const seen = new Set(rows.map((row) => row.id));
  const out = rows.slice();
  for (const row of items) {
    if (seen.has(row.id)) continue;
    seen.add(row.id);
    out.push(row);
  }
  return out;
}

/**
 * The entry list, newest first: every listed journal, or only `journalId`.
 * Loads page 1 on focus and pages on with `loadMore()` (for `onEndReached`);
 * each later focus reloads the pages already loaded. Components use this,
 * never the native module.
 */
export function useEntries(journalId: string | null) {
  const vault = useVault();
  const [rows, setRows] = useState<EntryRow[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const [hasMore, setHasMore] = useState(false);
  // A reload (first load, focus, refresh) is running; `loadingMore` a next page.
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState(false);
  // Pages loaded for which journal; a new journal starts again at page 1.
  const loaded = useRef({ journalId, pages: 0 });
  // Bumped by each reload and on unmount: an older answer is dropped.
  const generation = useRef(0);
  // A reload or a next page is running; `loadMore()` waits for it to end.
  const inFlight = useRef(false);
  const creating = useRef(false);

  useEffect(() => {
    return () => {
      generation.current += 1;
    };
  }, []);

  function fail(e: unknown) {
    if (errorCode(e) === 'ERR_LOCKED') vault.markLocked();
    else setError(true);
  }

  const refresh = useCallback(async () => {
    const gen = ++generation.current;
    inFlight.current = true;
    setLoading(true);
    const same = loaded.current.journalId === journalId;
    const wanted = same ? Math.max(1, loaded.current.pages) : 1;
    try {
      let items: EntryRow[] = [];
      let page = 0;
      let last: EntryPage | null = null;
      // Stops early when the list got shorter since those pages were loaded.
      while (page < wanted && (last === null || last.hasMore)) {
        page += 1;
        last = await MemloreCore.listEntries(journalId, page);
        items = appendNew(items, last.items);
      }
      if (gen !== generation.current || last === null) return;
      for (const row of items) putCachedRow(row);
      loaded.current = { journalId, pages: page };
      setRows(items);
      setTotal(last.total);
      setHasMore(last.hasMore);
      setError(false);
    } catch (e) {
      if (gen === generation.current) fail(e);
    } finally {
      if (gen === generation.current) {
        inFlight.current = false;
        setLoading(false);
      }
    }
  }, [journalId]);

  useFocusEffect(
    useCallback(() => {
      void refresh();
    }, [refresh]),
  );

  const loadMore = useCallback(async () => {
    if (!hasMore || inFlight.current) return;
    const gen = generation.current;
    const page = loaded.current.pages + 1;
    inFlight.current = true;
    setLoadingMore(true);
    try {
      const next = await MemloreCore.listEntries(journalId, page);
      if (gen !== generation.current) return;
      for (const row of next.items) putCachedRow(row);
      loaded.current = { journalId, pages: page };
      setRows((prev) => appendNew(prev, next.items));
      setTotal(next.total);
      setHasMore(next.hasMore);
    } catch (e) {
      if (gen === generation.current) fail(e);
    } finally {
      // A reload that started meanwhile owns `inFlight` now.
      if (gen === generation.current) inFlight.current = false;
      setLoadingMore(false);
    }
  }, [hasMore, journalId]);

  /**
   * A new untitled entry with an empty body in this list's journal (or the
   * first listed one); resolves with its id, or null when it failed or a
   * creation is already running.
   */
  const createEntry = useCallback(async (): Promise<string | null> => {
    if (creating.current) return null;
    creating.current = true;
    try {
      return await MemloreCore.createEntry('', journalId);
    } catch (e) {
      fail(e);
      return null;
    } finally {
      creating.current = false;
    }
  }, [journalId]);

  return { rows, total, hasMore, loading, loadingMore, error, refresh, loadMore, createEntry };
}

/**
 * One entry, read through `getEntry`. `notFound` when the list no longer shows
 * it (deleted, locked or invisible); `failed` for any other error. ERR_LOCKED
 * shows the lock screen.
 */
export function useEntryDetail(id: string) {
  const vault = useVault();
  const [detail, setDetail] = useState<EntryDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [notFound, setNotFound] = useState(false);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let active = true;
    setDetail(null);
    setLoading(true);
    setNotFound(false);
    setFailed(false);
    MemloreCore.getEntry(id).then(
      (entry) => {
        if (!active) return;
        putCachedRow({
          id: entry.id,
          title: entry.title,
          previewText: entry.previewText,
          entryDate: entry.entryDate,
          emotion: entry.emotion,
          isFavorite: entry.isFavorite,
          journalId: entry.journalId,
          isLocked: entry.isLocked,
          isInvisible: entry.isInvisible,
        });
        setDetail(entry);
        setLoading(false);
      },
      (e: unknown) => {
        if (!active) return;
        setLoading(false);
        const code = errorCode(e);
        if (code === 'ERR_LOCKED') {
          vault.markLocked();
        } else if (code === 'ERR_NOT_FOUND') {
          removeCachedRow(id);
          setNotFound(true);
        } else {
          setFailed(true);
        }
      },
    );
    return () => {
      active = false;
    };
  }, [id]);

  return { detail, loading, notFound, failed };
}

/** Moving to Trash waits at most this long for the open entry's unsaved edits. */
const TRASH_FLUSH_MS = 3000; // the entry screen's own flush cap

/** What the entry screen's meta row shows and the actions below change. */
export interface EntryMeta {
  emotion: Emotion | null;
  isFavorite: boolean;
  /** Unix seconds. */
  entryDate: number;
  journalId: string;
  tags: Tag[];
}

/** The fields that are also on the list's row (tags are not). */
type RowField = 'emotion' | 'isFavorite' | 'entryDate' | 'journalId';

function metaOf(row: EntryRow, tags: Tag[]): EntryMeta {
  const { emotion, isFavorite, entryDate, journalId } = row;
  return { emotion, isFavorite, entryDate, journalId, tags };
}

/**
 * The open entry's meta (emotion, favorite, date, journal, tags) and the
 * actions that change it. `meta` starts from the list's cached row (tags
 * empty), then follows `detail` from `useEntryDetail` once it is read; null
 * while neither is known, and every action then resolves false.
 *
 * Every action shows its change at once, reverts it when the engine refuses,
 * updates the row cache on success, and resolves true when the change was
 * stored. It never rejects:
 * - ERR_LOCKED shows the lock screen;
 * - ERR_NOT_FOUND for the entry sets `notFound` (the screen goes back, as for
 *   `useEntryDetail`) and drops its cached row. `move` and `addTag` also get
 *   it for a journal or tag that is gone: they ask `getEntry` and set
 *   `notFound` only when the entry is gone too; otherwise they just resolve
 *   false (refresh the picker).
 * - anything else just resolves false.
 */
export function useEntryActions(id: string, detail: EntryDetail | null) {
  const vault = useVault();
  const [meta, setMeta] = useState<EntryMeta | null>(() => {
    const row = getCachedRow(id);
    return row ? metaOf(row, []) : null;
  });
  const [notFound, setNotFound] = useState(false);
  // The current meta, read by actions that run before React re-renders.
  const metaRef = useRef(meta);
  // Bumped by each change of a field: only the latest change settles or reverts it.
  const seqs = useRef<Record<RowField, number>>({
    emotion: 0,
    isFavorite: 0,
    entryDate: 0,
    journalId: 0,
  });
  const trashing = useRef(false);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    if (!detail) return;
    metaRef.current = metaOf(detail, detail.tags);
    setMeta(metaRef.current);
  }, [detail]);

  function update(fn: (m: EntryMeta) => EntryMeta) {
    if (!metaRef.current) return;
    metaRef.current = fn(metaRef.current);
    if (mounted.current) setMeta(metaRef.current);
  }

  /** Whether `getEntry` still reads the entry (anything but ERR_NOT_FOUND counts as yes). */
  async function entryListed(): Promise<boolean> {
    try {
      await MemloreCore.getEntry(id);
      return true;
    } catch (e) {
      return errorCode(e) !== 'ERR_NOT_FOUND';
    }
  }

  /** `other`: the call also names a journal or tag, which may be the one gone. */
  async function fail(e: unknown, other: boolean) {
    const code = errorCode(e);
    if (code === 'ERR_LOCKED') {
      vault.markLocked();
      return;
    }
    if (code !== 'ERR_NOT_FOUND' || (other && (await entryListed()))) return;
    removeCachedRow(id);
    if (mounted.current) setNotFound(true);
  }

  /** Shows `value` at once; `call` resolves with the stored value when the engine says it. */
  async function change<K extends RowField>(
    field: K,
    value: EntryMeta[K],
    call: () => Promise<EntryMeta[K] | void>,
    other = false,
  ): Promise<boolean> {
    if (!metaRef.current) return false;
    const before = metaRef.current[field];
    const seq = ++seqs.current[field];
    update((m) => ({ ...m, [field]: value }));
    try {
      const result = await call();
      const stored = result === undefined ? value : result;
      if (seq === seqs.current[field]) update((m) => ({ ...m, [field]: stored }));
      const row = getCachedRow(id);
      if (row) putCachedRow({ ...row, [field]: stored });
      return true;
    } catch (e) {
      if (seq === seqs.current[field]) update((m) => ({ ...m, [field]: before }));
      await fail(e, other);
      return false;
    }
  }

  /** null clears the emotion. */
  const setEmotion = useCallback(
    (emotion: Emotion | null) =>
      change('emotion', emotion, () => MemloreCore.setEntryEmotion(id, emotion)),
    [id],
  );

  /** Settles on the engine's value, so quick taps never drift. */
  const toggleFavorite = useCallback(async () => {
    if (!metaRef.current) return false;
    const next = !metaRef.current.isFavorite;
    return change('isFavorite', next, () => MemloreCore.toggleEntryFavorite(id));
  }, [id]);

  /** Unix seconds; a fraction is dropped. */
  const setDate = useCallback(
    (unixSeconds: number) => {
      const secs = Math.floor(unixSeconds);
      return change('entryDate', secs, () => MemloreCore.setEntryDate(id, secs));
    },
    [id],
  );

  /** Into another listed journal. */
  const move = useCallback(
    (journalId: string) =>
      change('journalId', journalId, () => MemloreCore.moveEntry(id, journalId), true),
    [id],
  );

  /** Takes the whole tag so its pill shows its name at once. */
  const addTag = useCallback(
    async (tag: Tag) => {
      if (!metaRef.current) return false;
      if (metaRef.current.tags.some((t) => t.id === tag.id)) return true;
      update((m) => ({ ...m, tags: [...m.tags, tag] }));
      try {
        await MemloreCore.addTagToEntry(id, tag.id);
        return true;
      } catch (e) {
        update((m) => ({ ...m, tags: m.tags.filter((t) => t.id !== tag.id) }));
        await fail(e, true);
        return false;
      }
    },
    [id],
  );

  const removeTag = useCallback(
    async (tagId: string) => {
      const tag = metaRef.current?.tags.find((t) => t.id === tagId);
      if (!tag) return false;
      update((m) => ({ ...m, tags: m.tags.filter((t) => t.id !== tagId) }));
      try {
        await MemloreCore.removeTagFromEntry(id, tagId);
        return true;
      } catch (e) {
        update((m) => (m.tags.some((t) => t.id === tagId) ? m : { ...m, tags: [...m.tags, tag] }));
        await fail(e, false);
        return false;
      }
    },
    [id],
  );

  /**
   * Saves every open edit first (at most 2 s), then moves the entry to Trash
   * and drops its cached row. The caller goes back; the leave must not flush
   * again (the entry is gone, so a save would answer ERR_NOT_FOUND).
   */
  const trash = useCallback(async () => {
    if (trashing.current) return false;
    trashing.current = true;
    try {
      await flushAll(TRASH_FLUSH_MS);
      await MemloreCore.trashEntry(id);
      removeCachedRow(id);
      return true;
    } catch (e) {
      await fail(e, false);
      return false;
    } finally {
      trashing.current = false;
    }
  }, [id]);

  return { meta, notFound, setEmotion, toggleFavorite, setDate, move, addTag, removeTag, trash };
}
