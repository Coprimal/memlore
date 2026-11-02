import type { JSONContent } from '@tiptap/core'

import type { Template } from '../modules/memlore-core'
import type { TKey } from '../src/i18n'

/**
 * The predefined rows store their slug in `name` (engine `db/seeds.rs`
 * SEEDS); their text lives in `editor:templates.<slug>.*`. A fixed list,
 * because mobile `t()` answers a missing key with the key itself.
 */
const PREDEFINED_SLUGS = ['blank', 'daily-reflection', 'morning-pages'] as const
type PredefinedSlug = (typeof PREDEFINED_SLUGS)[number]

export function isPredefinedSlug(name: string): name is PredefinedSlug {
  return (PREDEFINED_SLUGS as readonly string[]).includes(name)
}

/** What a template puts into an empty entry: HTML, or typed ProseMirror nodes. */
export type TemplateBody = { kind: 'html'; html: string } | { kind: 'doc'; doc: JSONContent }

/**
 * The `exec('setContent', JSON.stringify(payload))` argument: with
 * `onlyIfEmpty`, the editor applies it only while the entry is still empty.
 */
export type SetContentPayload = TemplateBody & { onlyIfEmpty: boolean }

/**
 * The body desktop's `Editor.tsx` applies for a template, in the same order:
 * a predefined template's i18n HTML, a user template's stored HTML, else an
 * H1 with the name plus a paragraph with the description (typed nodes, never
 * HTML interpolation), else nothing.
 */
export function templateBody(row: Template, t: (key: TKey) => string): TemplateBody | null {
  let html: string | null = null
  if (row.isPredefined) {
    if (isPredefinedSlug(row.name)) {
      const content = t(`editor:templates.${row.name}.content`)
      html = content.trim().length > 0 ? content : null
    }
  } else if (row.contentHtml !== null && row.contentHtml.length > 0) {
    html = row.contentHtml
  }
  if (html !== null) return { kind: 'html', html }
  if (!row.description) return null
  return {
    kind: 'doc',
    doc: {
      type: 'doc',
      content: [
        { type: 'heading', attrs: { level: 1 }, content: [{ type: 'text', text: row.name }] },
        { type: 'paragraph', content: [{ type: 'text', text: row.description }] },
      ],
    },
  }
}
