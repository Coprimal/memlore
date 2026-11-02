import { useFocusEffect } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';

import MemloreCore, {
  type EntryDetail,
  type EntryPage,
  type EntryRow,
} from '@/modules/memlore-core';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { putCachedRow, removeCachedRow } from '@/src/lib/rowCache';

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
