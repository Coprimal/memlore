import { describe, expect, it } from 'vitest';
import { groupByDay } from './dayGroups';

// Local-time dates, so the midnight boundary holds in any runner time zone.
const at = (y: number, m: number, d: number, h = 0, min = 0, s = 0) =>
  new Date(y, m - 1, d, h, min, s);
const secs = (date: Date) => Math.floor(date.getTime() / 1000);

describe('groupByDay', () => {
  it.each([
    ['local midnight today', at(2026, 11, 2), 'today'],
    ['one second before local midnight', at(2026, 11, 1, 23, 59, 59), 'yesterday'],
    ['the start of yesterday', at(2026, 11, 1), 'yesterday'],
    ['the last second of the day before', at(2026, 10, 31, 23, 59, 59), '2026-10-31'],
  ])('puts %s in the right section', (_, date, key) => {
    const now = at(2026, 11, 2, 0, 30);
    expect(groupByDay([{ entryDate: secs(date) }], now)[0].key).toBe(key);
  });
});
