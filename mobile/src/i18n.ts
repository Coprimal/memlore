// Minimal lookup over the desktop's shared strings (src/locales, watched by
// Metro). English by default, Vietnamese when the device language is.
import enAuth from '../../src/locales/en/auth.json';
import enCommon from '../../src/locales/en/common.json';
import enEditor from '../../src/locales/en/editor.json';
import enNav from '../../src/locales/en/nav.json';
import enSettings from '../../src/locales/en/settings.json';
import viAuth from '../../src/locales/vi/auth.json';
import viCommon from '../../src/locales/vi/common.json';
import viEditor from '../../src/locales/vi/editor.json';
import viNav from '../../src/locales/vi/nav.json';
import viSettings from '../../src/locales/vi/settings.json';

type Leaves<T, P extends string = ''> = {
  [K in keyof T & string]: T[K] extends string
    ? `${P}${K}`
    : T[K] extends object
      ? Leaves<T[K], `${P}${K}.`>
      : never;
}[keyof T & string];

// i18next plural variants (`x_one`, `x_other`, `x_zero`) are asked for by the
// bare key plus a numeric `count`, so the bare key is a valid key too.
// TODO(later): require `count` for plural-only keys (docs/LATER.md "Mobile M3a notes").
type PluralBase<K extends string> = K extends `${infer B}_one`
  ? B
  : K extends `${infer B}_other`
    ? B
    : K extends `${infer B}_zero`
      ? B
      : never;

type Keys<T> = Leaves<T> | PluralBase<Leaves<T>>;

/** A key of auth.json, unprefixed (the mobile.* strings live there). */
export type AuthKey = Keys<typeof enAuth>;

/**
 * Any key `t()` accepts: an unprefixed auth key, or `ns:key` for the desktop
 * namespaces mobile reuses (`mini_date_picker` exists in both common and editor).
 */
export type TKey =
  | AuthKey
  | `nav:${Keys<typeof enNav>}`
  | `editor:${Keys<typeof enEditor>}`
  | `common:${Keys<typeof enCommon>}`
  | `settings:${Keys<typeof enSettings>}`;

export type Table = { locale: string; strings: Record<string, unknown> };

const EN: Table = {
  locale: 'en',
  strings: { auth: enAuth, nav: enNav, editor: enEditor, common: enCommon, settings: enSettings },
};
const VI: Table = {
  locale: 'vi',
  strings: { auth: viAuth, nav: viNav, editor: viEditor, common: viCommon, settings: viSettings },
};

const ACTIVE = Intl.DateTimeFormat().resolvedOptions().locale.toLowerCase().startsWith('vi')
  ? VI
  : EN;

/** The language `t()` reads ('en' or 'vi'), for `Intl` dates in the same language. */
export const activeLocale = ACTIVE.locale;

function lookup(strings: unknown, path: string): string | undefined {
  let node = strings;
  for (const part of path.split('.')) {
    if (typeof node !== 'object' || node === null) return undefined;
    node = (node as Record<string, unknown>)[part];
  }
  return typeof node === 'string' ? node : undefined;
}

function pluralCategory(locale: string, count: number): string {
  // Hermes may ship without Intl.PluralRules; en and vi need only one/other.
  if (typeof Intl.PluralRules === 'function') return new Intl.PluralRules(locale).select(count);
  return count === 1 ? 'one' : 'other';
}

// Matches i18next: `_zero` wins for 0 when it exists, then the CLDR category,
// then `_other`, then the bare key.
function resolve(table: Table, path: string, count: unknown): string | undefined {
  if (typeof count === 'number') {
    const variant =
      (count === 0 ? lookup(table.strings, `${path}_zero`) : undefined) ??
      lookup(table.strings, `${path}_${pluralCategory(table.locale, count)}`) ??
      lookup(table.strings, `${path}_other`);
    if (variant !== undefined) return variant;
  }
  return lookup(table.strings, path);
}

/** `t()` over explicit tables, tried in order (the active language, then en). */
export function translate(
  tables: readonly Table[],
  key: string,
  vars?: Record<string, string | number>,
): string {
  const sep = key.indexOf(':');
  const path = sep < 0 ? `auth.${key}` : `${key.slice(0, sep)}.${key.slice(sep + 1)}`;
  let raw: string | undefined;
  for (const table of tables) {
    raw = resolve(table, path, vars?.count);
    if (raw !== undefined) break;
  }
  if (raw === undefined) return key;
  if (!vars) return raw;
  return raw.replace(/\{\{(\w+)\}\}/g, (match, name: string) =>
    name in vars ? String(vars[name]) : match,
  );
}

export function t(key: TKey, vars?: Record<string, string | number>): string {
  return translate(ACTIVE === EN ? [EN] : [ACTIVE, EN], key, vars);
}
