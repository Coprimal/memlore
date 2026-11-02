import type { EntryRow } from '@/modules/memlore-core';

// Every row a list or an entry read has seen, by id: the entry screen shows its
// title and preview at once from here, before `getEntry` answers. Cleared and
// closed while the vault is locked (useVault), so no row outlives the lock and
// a read that answers after the lock cannot put one back.
const rows = new Map<string, EntryRow>();
let open = false;

export function getCachedRow(id: string): EntryRow | undefined {
  return rows.get(id);
}

export function putCachedRow(row: EntryRow): void {
  if (open) rows.set(row.id, row);
}

export function removeCachedRow(id: string): void {
  rows.delete(id);
}

/** The vault locked: drop every row and take no new ones. */
export function clearRowCache(): void {
  open = false;
  rows.clear();
}

/** The vault unlocked: rows can be cached again. */
export function enableRowCache(): void {
  open = true;
}
