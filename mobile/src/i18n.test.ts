import { describe, expect, it } from 'vitest';
import { type Table, translate } from './i18n';

// The plural and namespace resolution mirrors i18next (desktop), so the same
// JSON reads the same on both apps.
const EN: Table = {
  locale: 'en',
  strings: {
    auth: { mobile: { lock: 'Lock', min: 'At least {{count}}' } },
    common: { mini_date_picker: { today: 'Today (common)' } },
    editor: { mini_date_picker: { today: 'Today (editor)' } },
    settings: {
      days_one: '{{count}} day',
      days_other: '{{count}} days',
      ago_zero: 'today',
      ago_one: '{{count}} day ago',
      ago_other: '{{count}} days ago',
      en_only: 'English only',
    },
  },
};
// Vietnamese has a single plural category, so desktop vi ships only `_other`.
const VI: Table = {
  locale: 'vi',
  strings: { auth: { mobile: { lock: 'Khóa' } }, settings: { days_other: '{{count}} ngày' } },
};

describe('translate', () => {
  it.each([
    [[EN], 'mobile.lock', undefined, 'Lock'],
    [[EN], 'common:mini_date_picker.today', undefined, 'Today (common)'],
    [[EN], 'editor:mini_date_picker.today', undefined, 'Today (editor)'],
    [[EN], 'settings:days', { count: 1 }, '1 day'],
    [[EN], 'settings:days', { count: 0 }, '0 days'],
    [[EN], 'settings:ago', { count: 0 }, 'today'],
    [[EN], 'settings:ago', { count: 2 }, '2 days ago'],
    [[EN], 'mobile.min', { count: 8 }, 'At least 8'],
    [[VI, EN], 'settings:days', { count: 1 }, '1 ngày'],
    [[VI, EN], 'settings:en_only', undefined, 'English only'],
    [[VI, EN], 'mobile.lock', undefined, 'Khóa'],
    [[VI, EN], 'mobile.missing', undefined, 'mobile.missing'],
  ] as const)('%#: %s', (tables, key, vars, expected) => {
    expect(translate(tables, key, vars)).toBe(expected);
  });
});
