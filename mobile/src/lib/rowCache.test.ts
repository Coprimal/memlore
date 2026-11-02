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
});
