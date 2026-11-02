/** A day section's key: today, yesterday, or the local `YYYY-MM-DD` of an older day. */
export type DayKey = 'today' | 'yesterday' | `${number}-${string}-${string}`;

export interface DayGroup<T> {
  key: DayKey;
  items: T[];
}

function localDay(date: Date): string {
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${date.getFullYear()}-${month}-${day}`;
}

/**
 * Groups rows (newest first) by the local day of `entryDate` (unix seconds),
 * like the desktop's `groupEntriesByDate` (src/lib/dates.ts). Yesterday is
 * the previous calendar day, not 24 hours back, so it stays right across a
 * daylight-saving change. Groups keep the input order.
 */
export function groupByDay<T extends { entryDate: number }>(
  rows: readonly T[],
  now: Date,
): DayGroup<T>[] {
  const today = localDay(now);
  const yesterday = localDay(new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1));
  const groups = new Map<DayKey, T[]>();
  for (const row of rows) {
    const day = localDay(new Date(row.entryDate * 1000));
    const key: DayKey =
      day === today ? 'today' : day === yesterday ? 'yesterday' : (day as DayKey);
    const items = groups.get(key);
    if (items) items.push(row);
    else groups.set(key, [row]);
  }
  return Array.from(groups, ([key, items]) => ({ key, items }));
}
