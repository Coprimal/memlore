import { describe, expect, it } from 'vitest';
import type { EntryRow } from '@/modules/memlore-core';
import { clearRowCache, enableRowCache, getCachedRow, putCachedRow } from './rowCache';

const row = { id: 'e1', title: 'Title' } as EntryRow;

// Fail closed: titles and previews must never be cached while the vault is
// locked, including by a read that answers after the lock.
describe('rowCache', () => {
  it('takes rows only while the vault is unlocked', () => {
    putCachedRow(row);
    expect(getCachedRow('e1')).toBeUndefined();

    enableRowCache();
    putCachedRow(row);
    expect(getCachedRow('e1')).toBe(row);

    clearRowCache();
    expect(getCachedRow('e1')).toBeUndefined();
    putCachedRow(row);
    expect(getCachedRow('e1')).toBeUndefined();
  });

  // A revealed locked row or an open-vault row must never outlive the reveal
  // or the vault in the warm-start cache (M3c).
  it('never caches a locked or invisible row, and drops a row that became one', () => {
    enableRowCache();
    putCachedRow({ ...row, isLocked: true });
    expect(getCachedRow('e1')).toBeUndefined();
    putCachedRow({ ...row, isInvisible: true });
    expect(getCachedRow('e1')).toBeUndefined();

    const plain = { ...row, isLocked: false, isInvisible: false };
    putCachedRow(plain);
    expect(getCachedRow('e1')).toBe(plain);
    putCachedRow({ ...plain, isLocked: true });
    expect(getCachedRow('e1')).toBeUndefined();
    clearRowCache();
  });
});
