import { useFocusEffect } from 'expo-router';
import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';

import MemloreCore, {
  type Emotion,
  type EntryDetail,
  type EntryPage,
  type EntryRow,
  type MemloreErrorCode,
  type Tag,
} from '@/modules/memlore-core';
import { useLockSession } from '@/src/hooks/useLockSession';
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

// Bumped by every lock or invisibility change of an entry or journal (and
// every move): rows read before it may show an entry now locked as plain, so
// lists drop them.
let listsDirty = 0;
const dirtyListeners = new Set<() => void>();

function subscribeDirty(listener: () => void): () => void {
  dirtyListeners.add(listener);
  return () => dirtyListeners.delete(listener);
}

const readDirty = () => listsDirty;

/** An entry or journal lock changed: every entry list reads again (empty until then). */
export function markEntryListsDirty(): void {
  listsDirty += 1;
  for (const listener of dirtyListeners) listener();
}

/** Counts `markEntryListsDirty` calls: a list read before a change must not be shown. */
export function useEntryListsDirty(): number {
  return useSyncExternalStore(subscribeDirty, readDirty, readDirty);
}

/** Returned in place of rows read before the last lock change. */
const NO_ROWS: EntryRow[] = [];

/**
 * The entry list, newest first: every listed journal, or only `journalId`.
 * Loads page 1 on focus and pages on with `loadMore()` (for `onEndReached`);
 * each later focus reloads the pages already loaded, and so does a change of
 * the lock session's `generation` (a reveal or re-lock). Components use this,
 * never the native module. `failure` is the last error's code; a journal-only
 * list refused with ERR_SECOND_LOCK_REQUIRED or ERR_NOT_FOUND (the journal is
 * locked or gone) empties instead of setting `error`.
 *
 * Rows read before the last reveal, re-lock or lock change
 * (`markEntryListsDirty`) are never returned (fail closed): `rows` is empty,
 * `total` null and `loading` true until a read in the current state succeeds,
 * or empty with `error` when it failed.
 */
export function useEntries(journalId: string | null) {
  const vault = useVault();
  // A new reveal, re-lock or lock change gives `refresh` a new identity, so
  // the focus effect below reloads at once (or on the next focus).
  const { generation: lockGeneration } = useLockSession();
  const dirty = useEntryListsDirty();
  const lockState = `${lockGeneration}:${dirty}`;
  // The same, for a read that must say which lock state it ran in.
  const lockStateRef = useRef(lockState);
  lockStateRef.current = lockState;
  // The lock state `rows` and `total` were read in.
  const [readIn, setReadIn] = useState(lockState);
  const readInRef = useRef(readIn);
  readInRef.current = readIn;
  const [rows, setRows] = useState<EntryRow[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const [hasMore, setHasMore] = useState(false);
  // A reload (first load, focus, refresh) is running; `loadingMore` a next page.
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState(false);
  // The last failure's module code (null after a success or a non-module error).
  const [failure, setFailure] = useState<MemloreErrorCode | null>(null);
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

  /** `lockAt`: the lock state the failed call started in. */
  function fail(e: unknown, lockAt: string) {
    const code = errorCode(e);
    if (code === 'ERR_LOCKED') {
      vault.markLocked();
      return;
    }
    setFailure(code);
    if (journalId !== null && (code === 'ERR_SECOND_LOCK_REQUIRED' || code === 'ERR_NOT_FOUND')) {
      // The filtered journal is locked or gone: its rows (maybe read while
      // revealed) are dropped, and the screen falls back to All journals.
      setRows([]);
      setTotal(null);
      setHasMore(false);
      // An older failure must not show during the fallback.
      setError(false);
      // Empty is this lock state's answer: the screen may fall back now.
      setReadIn(lockAt);
      return;
    }
    setError(true);
  }

  const refresh = useCallback(async () => {
    const gen = ++generation.current;
    const lockAt = lockStateRef.current;
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
      // Read before a lock change that landed meanwhile: not cached, never shown.
      if (lockAt === lockStateRef.current) for (const row of items) putCachedRow(row);
      loaded.current = { journalId, pages: page };
      setRows(items);
      setReadIn(lockAt);
      setTotal(last.total);
      setHasMore(last.hasMore);
      setError(false);
      setFailure(null);
    } catch (e) {
      if (gen === generation.current) fail(e, lockAt);
    } finally {
      if (gen === generation.current) {
        inFlight.current = false;
        setLoading(false);
      }
    }
  }, [journalId, lockState]);

  useFocusEffect(
    useCallback(() => {
      void refresh();
    }, [refresh]),
  );

  const loadMore = useCallback(async () => {
    if (!hasMore || inFlight.current) return;
    const lockAt = lockStateRef.current;
    // The rows shown are from an older lock state: only a reload may replace them.
    if (readInRef.current !== lockAt) return;
    const gen = generation.current;
    const page = loaded.current.pages + 1;
    inFlight.current = true;
    setLoadingMore(true);
    try {
      const next = await MemloreCore.listEntries(journalId, page);
      // A page from an older lock state is dropped.
      if (gen !== generation.current || lockAt !== lockStateRef.current) return;
      for (const row of next.items) putCachedRow(row);
      loaded.current = { journalId, pages: page };
      setRows((prev) => appendNew(prev, next.items));
      setTotal(next.total);
      setHasMore(next.hasMore);
    } catch (e) {
      if (gen === generation.current) fail(e, lockAt);
    } finally {
      // A reload that started meanwhile owns `inFlight` now.
      if (gen === generation.current) inFlight.current = false;
      setLoadingMore(false);
    }
  }, [hasMore, journalId]);

  /**
   * A new untitled entry with an empty body in this list's journal (or the
   * engine's first listed journal outside any vault); resolves with its id, or
   * null when it failed or a creation is already running. A list filtered on a
   * vault journal passes it explicitly, so the entry is created in the vault.
   */
  const createEntry = useCallback(async (): Promise<string | null> => {
    if (creating.current) return null;
    creating.current = true;
    try {
      return await MemloreCore.createEntry('', journalId);
    } catch (e) {
      fail(e, lockStateRef.current);
      return null;
    } finally {
      creating.current = false;
    }
  }, [journalId]);

  // Read before the last lock change: never shown.
  const stale = readIn !== lockState;
  return {
    rows: stale ? NO_ROWS : rows,
    total: stale ? null : total,
    hasMore: stale ? false : hasMore,
    loading: loading || (stale && !error),
    loadingMore,
    error,
    failure,
    refresh,
    loadMore,
    createEntry,
  };
}

/**
 * One entry, read through `getEntry`. `notFound` when the list no longer shows
 * it (deleted, locked or invisible); `lockRequired` when it is locked and
 * listed redacted (ERR_SECOND_LOCK_REQUIRED: reveal, then bump `attempt` to
 * read again); `failed` for any other error. ERR_LOCKED shows the lock screen.
 */
export function useEntryDetail(id: string, attempt = 0) {
  const vault = useVault();
  const [detail, setDetail] = useState<EntryDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [notFound, setNotFound] = useState(false);
  const [lockRequired, setLockRequired] = useState(false);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let active = true;
    setDetail(null);
    setLoading(true);
    setNotFound(false);
    setLockRequired(false);
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
        } else if (code === 'ERR_SECOND_LOCK_REQUIRED') {
          setLockRequired(true);
        } else {
          setFailed(true);
        }
      },
    );
    return () => {
      active = false;
    };
  }, [id, attempt]);

  return { detail, loading, notFound, lockRequired, failed };
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

/** An entry's second lock and invisibility, its journal's included. */
export interface EntryFlags {
  locked: boolean;
  invisible: boolean;
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
      change(
        'journalId',
        journalId,
        async () => {
          await MemloreCore.moveEntry(id, journalId);
          // Into a locked journal, the entry is locked with it.
          markEntryListsDirty();
        },
        true,
      ),
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

  /**
   * Whether the entry is locked and invisible now, its journal's flags
   * included (e.g. after a move). A refusal for the lock or a gone entry
   * (ERR_SECOND_LOCK_REQUIRED, ERR_NOT_FOUND, ERR_LOCKED) counts as both;
   * null when the read failed otherwise (callers count it as both too, after
   * saving what is open).
   */
  const readFlags = useCallback(async (): Promise<EntryFlags | null> => {
    try {
      const entry = await MemloreCore.getEntry(id);
      return { locked: entry.isLocked, invisible: entry.isInvisible };
    } catch (e) {
      const code = errorCode(e);
      const refused =
        code === 'ERR_SECOND_LOCK_REQUIRED' || code === 'ERR_NOT_FOUND' || code === 'ERR_LOCKED';
      return refused ? { locked: true, invisible: true } : null;
    }
  }, [id]);

  /**
   * Locks or unlocks the entry with the second lock and resolves whether it
   * is locked now (its journal's lock still counts after an unlock; an entry
   * that cannot be read back counts as locked). Locking saves every open edit
   * first (at most 2 s): once locked, a save while not revealed is refused.
   * Unlike the other actions it rejects with the module's error, so the screen
   * can ask for the reveal (ERR_SECOND_LOCK_REQUIRED) or show the engine's
   * text (ERR_IO, e.g. an entry in Trash); ERR_LOCKED and ERR_NOT_FOUND are
   * also handled as above.
   */
  const setLocked = useCallback(
    async (locked: boolean): Promise<boolean> => {
      if (locked) await flushAll(TRASH_FLUSH_MS);
      try {
        await MemloreCore.setEntryLocked(id, locked);
      } catch (e) {
        await fail(e, false);
        throw e;
      }
      markEntryListsDirty();
      if (locked) {
        // Its plain copy must not outlive the lock (the cache never holds a locked row).
        removeCachedRow(id);
        return true;
      }
      return (await readFlags())?.locked ?? true;
    },
    [id],
  );

  /**
   * Moves the entry into the open invisible vault, or out of it, and resolves
   * its flags read back (null when that read failed): moving in clears its
   * own second lock (engine rule), but a locked or invisible journal still
   * counts. Moving in needs an open vault. It stays listed while the vault is
   * open, so no save is needed first. Like `setLocked` it rejects with the
   * module's error (any refusal but ERR_LOCKED and ERR_NOT_FOUND is the
   * generic "Unable to update"); those two are also handled as above.
   */
  const setInvisible = useCallback(
    async (invisible: boolean): Promise<EntryFlags | null> => {
      try {
        await MemloreCore.setEntryInvisible(id, invisible);
      } catch (e) {
        await fail(e, false);
        throw e;
      }
      markEntryListsDirty();
      // The cache never holds an invisible row.
      if (invisible) removeCachedRow(id);
      return readFlags();
    },
    [id],
  );

  return {
    meta,
    notFound,
    setEmotion,
    toggleFavorite,
    setDate,
    move,
    addTag,
    removeTag,
    trash,
    setLocked,
    setInvisible,
    readFlags,
  };
}
