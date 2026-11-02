import { generateJSON, getSchema, type JSONContent } from '@tiptap/core'
import { describe, expect, it } from 'vitest'

import enEditor from '../../src/locales/en/editor.json'
import viEditor from '../../src/locales/vi/editor.json'
import type { Template } from '../modules/memlore-core'
import { translate, type TKey } from '../src/i18n'
import { buildMobileExtensions } from './extensions'
import { templateBody } from './templateBody'

// Template apply must land in the mobile schema the way desktop's Editor.tsx
// lands it in the desktop one: the editor would silently drop any block it
// cannot hold, and the body is then written into the entry's Yjs document.

const LOCALES = [
  ['en', enEditor],
  ['vi', viEditor],
] as const
const SLUGS = ['blank', 'daily-reflection', 'morning-pages'] as const

function tFor(locale: string, editor: object): (key: TKey) => string {
  return (key) => translate([{ locale, strings: { editor } }], key)
}

function row(fields: Partial<Template>): Template {
  return {
    id: 'id',
    name: '',
    description: null,
    contentHtml: null,
    isPredefined: false,
    sortOrder: 0,
    ...fields,
  }
}

/** The top-level blocks, as `h2` / `p` like the source HTML names them. */
function blocks(doc: JSONContent): string[] {
  return (doc.content ?? []).map((node) =>
    node.type === 'heading' ? `h${String(node.attrs?.level)}` : (node.type ?? ''),
  )
}

const cases = LOCALES.flatMap(([locale, editor]) =>
  SLUGS.map((slug) => [locale, slug, editor] as const),
)

describe('templateBody', () => {
  it.each(cases)('predefined %s/%s keeps every h2 and p in the schema', (locale, slug, e) => {
    const template = row({ name: slug, isPredefined: true, description: '' })
    const body = templateBody(template, tFor(locale, e))
    if (body?.kind !== 'html') throw new Error(`expected html, got ${JSON.stringify(body)}`)
    const expected = [...body.html.matchAll(/<(h2|p)>/g)].map((m) =>
      m[1] === 'p' ? 'paragraph' : 'h2',
    )
    expect(expected.length).toBeGreaterThan(0)
    const parsed = generateJSON(body.html, buildMobileExtensions())
    expect(blocks(parsed)).toEqual(expected)
  })

  const t = tFor('en', enEditor)
  it.each([
    [
      'user content html',
      row({ name: 'n', contentHtml: '<p>hi</p>' }),
      { kind: 'html', html: '<p>hi</p>' },
    ],
    [
      'user fallback with markup kept literal',
      row({ name: '<b>x</b>', description: '<i>d</i> & <script>' }),
      {
        kind: 'doc',
        doc: {
          type: 'doc',
          content: [
            {
              type: 'heading',
              attrs: { level: 1 },
              content: [{ type: 'text', text: '<b>x</b>' }],
            },
            { type: 'paragraph', content: [{ type: 'text', text: '<i>d</i> & <script>' }] },
          ],
        },
      },
    ],
    ['empty user template', row({ name: 'n', contentHtml: '', description: '' }), null],
    ['user template without description', row({ name: 'n' }), null],
    ['unknown predefined slug', row({ name: 'gone', isPredefined: true, description: '' }), null],
  ])('%s', (_name, template, expected) => {
    const body = templateBody(template, t)
    expect(body).toEqual(expected)
    if (body?.kind === 'doc') {
      // The typed nodes are valid in the mobile schema, the text untouched.
      const node = getSchema(buildMobileExtensions()).nodeFromJSON(body.doc)
      node.check()
      expect(node.child(0).textContent).toBe(template.name)
    }
  })
})
