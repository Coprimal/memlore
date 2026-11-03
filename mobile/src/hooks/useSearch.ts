import { useFocusEffect } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';

import MemloreCore, { type Emotion, type SearchResult } from '@/modules/memlore-core';
import { useEntryListsDirty } from '@/src/hooks/useEntries';
import { useLockSession } from '@/src/hooks/useLockSession';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { type RangePreset, rangeBounds } from '@/src/lib/searchText';

/** How long typing pauses before a search runs. */
const DEBOUNCE_MS = 300;

/** The search screen's filters; an empty list (or `all`) means no restriction. */
export interface SearchUiFilters {
  /** One journal, or null for every listed journal. */
  journalId: string | null;
  tagIds: string[];
  emotions: Emotion[];
  range: RangePreset;
}

export function hasAnyFilter(filters: SearchUiFilters): boolean {
  return (
    filters.journalId !== null ||
    filters.tagIds.length > 0 ||
    filters.emotions.length > 0 ||
    filters.range !== 'all'
  );
}

/** The answer for one query and filter set (`key`), read in lock state `lockState`. */
type Done = { key: string; lockState: string; results: SearchResult[]; error: boolean };

const NONE: Done = { key: '', lockState: '', results: [], error: false };

/**
 * Keyword search (at most 50 hits): runs 300 ms after the trimmed query or the
 * filters last changed; an older answer is dropped. `active` is false for a
 * blank query without a filter, which never calls the engine (no results).
 * `loading` holds until the answer for the current input arrives; the previous
 * results stay meanwhile. `error` is a failed search; ERR_LOCKED shows the
 * lock screen instead. A reveal or re-lock (the lock session's `generation`),
 * or a lock change of an entry or journal (`markEntryListsDirty`), drops the
 * results at once and runs the search again without the delay.
 */
export function useSearch(query: string, filters: SearchUiFilters) {
  const vault = useVault();
  const { generation: lockGeneration } = useLockSession();
  const lockState = `${lockGeneration}:${useEntryListsDirty()}`;
  // The lock state the last search ran for.
  const lockSeen = useRef(lockState);
  const [done, setDone] = useState<Done>(NONE);
  // Bumped by each new input and on unmount: an older answer is dropped.
  const generation = useRef(0);

  const trimmed = query.trim();
  const active = trimmed !== '' || hasAnyFilter(filters);
  // Stable across renders with equal filters, so they don't restart the delay.
  const key = JSON.stringify([trimmed, filters]);

  useEffect(() => {
    return () => {
      generation.current += 1;
    };
  }, []);

  // Back on this screen: run the same search again, since a result opened from
  // here may have been edited, retitled or trashed meanwhile.
  const [refreshTick, setRefreshTick] = useState(0);
  const focusedBefore = useRef(false);
  useFocusEffect(
    useCallback(() => {
      if (focusedBefore.current) setRefreshTick((n) => n + 1);
      focusedBefore.current = true;
    }, []),
  );

  useEffect(() => {
    const gen = ++generation.current;
    // Results read while revealed, or before a lock change, must not outlive
    // it (they carry no lock flag to filter on): they go now, and the search
    // runs at once.
    const lockChanged = lockSeen.current !== lockState;
    lockSeen.current = lockState;
    if (lockChanged) setDone(NONE);
    if (!active) {
      setDone(NONE);
      return;
    }
    const timer = setTimeout(() => {
      // The range is resolved when the search runs ("this week" as of now).
      const bounds = rangeBounds(filters.range);
      MemloreCore.searchEntries(trimmed, {
        from: bounds?.from ?? null,
        toExclusive: bounds?.toExclusive ?? null,
        journalIds: filters.journalId === null ? [] : [filters.journalId],
        tagIds: filters.tagIds,
        emotions: filters.emotions,
      }).then(
        (results) => {
          if (gen !== generation.current) return;
          setDone({ key, lockState, results, error: false });
        },
        (e: unknown) => {
          if (gen !== generation.current) return;
          if (errorCode(e) === 'ERR_LOCKED') vault.markLocked();
          else setDone({ key, lockState, results: [], error: true });
        },
      );
    }, lockChanged ? 0 : DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [key, refreshTick, lockState]);

  // Read before the last reveal, re-lock or lock change: never shown, not
  // even for the frame before the effect drops it.
  const fresh = done.lockState === lockState;
  const settled = fresh && done.key === key;
  return {
    results: active && fresh ? done.results : [],
    loading: active && !settled,
    error: active && settled && done.error,
    active,
  };
}
