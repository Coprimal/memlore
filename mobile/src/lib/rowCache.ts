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

/**
 * A locked or invisible row is never cached: its title and preview were read
 * only because the second lock was revealed or its vault was open, and must
 * not outlive either. Such a row also drops an older plain copy of the entry.
 */
export function putCachedRow(row: EntryRow): void {
  if (row.isLocked || row.isInvisible) {
    rows.delete(row.id);
    return;
  }
  if (open) rows.set(row.id, row);
}

export function removeCachedRow(id: string): void {
  rows.delete(id);
}

/**
 * A second-lock reveal or an invisible vault ended: drop every row, but keep
 * taking plain ones while the vault stays unlocked. Never reopens the cache,
 * so it is safe when the vault locked in the same tick.
 */
export function dropCachedRows(): void {
  rows.clear();
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
