import { describe, expect, it } from 'vitest';
import { highlightParts } from './searchText';

/** The matched parts only, in order. */
const marked = (text: string, query: string) =>
  highlightParts(text, query)
    .filter((part) => part.match)
    .map((part) => part.text);

// Folded matches must map back onto the original characters: NFD splits one
// vowel into up to three units and an emoji is two, so a naive index slips.
describe('highlightParts', () => {
  it.each([
    ['đ folds to d', 'Đi chơi đâu', 'di dau', ['Đi', 'đâu']],
    ['a vowel with two marks', 'Hôm nay Việt vui', 'viet', ['Việt']],
    ['after an emoji', '🎉🎉 Phở ngon', 'pho', ['Phở']],
    ['words out of order, overlaps merged', 'cà phê sữa', 'sua ca caphe ca', ['cà', 'sữa']],
  ])('%s', (_, text, query, expected) => {
    expect(marked(text, query)).toEqual(expected);
  });

  it('keeps every character of the text, in order', () => {
    const text = '🎉 Ngày Việt Nam, việc vui';
    expect(highlightParts(text, 'viet').map((part) => part.text).join('')).toBe(text);
  });
});
