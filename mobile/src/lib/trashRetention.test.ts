import { describe, expect, it } from 'vitest';
import { trashDaysLeft } from './trashRetention';

const DAY = 86_400;
const NOW = 1_800_000_000;

// The days left must match the desktop Trash for the same entry.
describe('trashDaysLeft', () => {
  it.each([
    ['just trashed', NOW, 30],
    ['a second short of one day', NOW - DAY + 1, 30],
    ['a second short of 30 days', NOW - 30 * DAY + 1, 1],
    ['exactly 30 days ago', NOW - 30 * DAY, 0],
    ['past the retention (sweep lagging)', NOW - 45 * DAY, 0],
    ['in the future (peer clock ahead)', NOW + 3 * DAY, 30],
  ])('%s', (_, trashedAt, left) => {
    expect(trashDaysLeft(trashedAt, NOW)).toBe(left);
  });
});
