// Pure helpers for the search screen: the time-range presets and the
// accent-insensitive match highlight.

/** The search screen's time-range presets (desktop's, without the custom range). */
export type RangePreset = 'all' | 'thisWeek' | 'thisMonth' | 'thisYear';

export const RANGE_PRESETS: readonly RangePreset[] = ['all', 'thisWeek', 'thisMonth', 'thisYear'];

/** Unix seconds: `from` inclusive, `toExclusive` exclusive. */
export interface RangeBounds {
  from: number;
  toExclusive: number;
}

const MS_PER_DAY = 24 * 60 * 60 * 1000;

function toSec(date: Date): number {
  return Math.floor(date.getTime() / 1000);
}

/**
 * The local-time bounds of `preset` around `nowMs`; null for `all`. A port of
 * desktop `timeRangeBounds` (src/lib/entryFilterSort.ts). The week starts on
 * `firstDayOfWeek` (a `Date.getDay()` value); Monday, as in the desktop search.
 */
export function rangeBounds(
  preset: RangePreset,
  nowMs: number = Date.now(),
  firstDayOfWeek = 1,
): RangeBounds | null {
  const now = new Date(nowMs);
  if (preset === 'thisWeek') {
    const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate());
    const offset = (startOfToday.getDay() - firstDayOfWeek + 7) % 7;
    const start = new Date(startOfToday.getTime() - offset * MS_PER_DAY);
    const end = new Date(start.getTime() + 7 * MS_PER_DAY);
    return { from: toSec(start), toExclusive: toSec(end) };
  }
  if (preset === 'thisMonth') {
    const start = new Date(now.getFullYear(), now.getMonth(), 1);
    const end = new Date(now.getFullYear(), now.getMonth() + 1, 1);
    return { from: toSec(start), toExclusive: toSec(end) };
  }
  if (preset === 'thisYear') {
    const start = new Date(now.getFullYear(), 0, 1);
    const end = new Date(now.getFullYear() + 1, 0, 1);
    return { from: toSec(start), toExclusive: toSec(end) };
  }
  return null;
}

/**
 * One character folded like the full-text index: decomposed, combining marks
 * dropped, `đ`/`Đ` as `d`, lower case ("Việt" → "viet").
 */
function foldChar(char: string): string {
  return char
    .toLowerCase()
    .normalize('NFD')
    .replace(/[\u0300-\u036f]/g, '')
    .replace(/đ/g, 'd');
}

/**
 * `text` folded, with the UTF-16 index in `text` of the character each folded
 * unit came from (one entry per folded unit, plus `text.length` at the end).
 */
function foldWithMap(text: string): { folded: string; source: number[] } {
  let folded = '';
  const source: number[] = [];
  let at = 0;
  // Code points, so a surrogate pair (an emoji) stays one character.
  for (const char of text) {
    const part = foldChar(char);
    folded += part;
    for (let i = 0; i < part.length; i++) source.push(at);
    at += char.length;
  }
  source.push(text.length);
  return { folded, source };
}

/** A highlighted span of the original text: `[start, end)` in UTF-16 units. */
export type HighlightRange = [start: number, end: number];

/**
 * Where the query's words occur in `text`, ignoring case and accents ("viet"
 * marks "Việt"), sorted and merged. Each whitespace-separated word is matched
 * on its own, as the full-text search matches them; `"` is dropped as there.
 */
export function highlightRanges(text: string, query: string): HighlightRange[] {
  const words = query
    .split(/\s+/)
    .map((word) => foldWithMap(word.replace(/"/g, '')).folded)
    .filter((word) => word !== '');
  if (words.length === 0 || text === '') return [];
  const { folded, source } = foldWithMap(text);
  const found: HighlightRange[] = [];
  for (const word of words) {
    let from = folded.indexOf(word);
    while (from !== -1) {
      const last = from + word.length - 1;
      // The end of the character the last folded unit came from.
      const end = source.find((index) => index > source[last]) ?? text.length;
      found.push([source[from], end]);
      from = folded.indexOf(word, from + word.length);
    }
  }
  found.sort((a, b) => a[0] - b[0]);
  const merged: HighlightRange[] = [];
  for (const range of found) {
    const prev = merged[merged.length - 1];
    if (prev && range[0] <= prev[1]) prev[1] = Math.max(prev[1], range[1]);
    else merged.push([range[0], range[1]]);
  }
  return merged;
}

/** `text` cut into plain and matched parts, in order (for nested `<Text>`). */
export function highlightParts(
  text: string,
  query: string,
): { text: string; match: boolean }[] {
  const parts: { text: string; match: boolean }[] = [];
  let at = 0;
  for (const [start, end] of highlightRanges(text, query)) {
    if (start > at) parts.push({ text: text.slice(at, start), match: false });
    parts.push({ text: text.slice(start, end), match: true });
    at = end;
  }
  if (at < text.length) parts.push({ text: text.slice(at), match: false });
  return parts;
}
