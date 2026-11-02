import { Editor, getSchema, type JSONContent } from '@tiptap/core'
import Collaboration from '@tiptap/extension-collaboration'
import { prosemirrorJSONToYDoc, yDocToProsemirrorJSON } from '@tiptap/y-tiptap'
import { afterEach, describe, expect, it } from 'vitest'
import * as Y from 'yjs'

import { restoreVersionCommand, setContentCommand } from './editorCommands'
import { buildMobileExtensions } from './extensions'

// The guards that keep `setContent` (a template) and `restoreVersion` from
// wiping an entry: each refused argument must change neither the Y.Doc nor
// the editor's document (which y-prosemirror writes back on the next edit).
// Same Collaboration-bound harness as openNoMutation.test.ts.

const editors: Editor[] = []
afterEach(() => {
  for (const editor of editors.splice(0)) editor.destroy()
})

const settle = () => new Promise((resolve) => setTimeout(resolve, 20))
const toBase64 = (bytes: Uint8Array) => Buffer.from(bytes).toString('base64')

function textDoc(heading: string, paragraph: string): JSONContent {
  return {
    type: 'doc',
    content: [
      { type: 'heading', attrs: { level: 1 }, content: [{ type: 'text', text: heading }] },
      { type: 'paragraph', content: [{ type: 'text', text: paragraph }] },
    ],
  }
}

const ENTRY = textDoc('My entry', 'Written before the command.')
const VERSION = textDoc('Old version', 'What the entry said then.')
const schema = getSchema(buildMobileExtensions())
const versionDoc = prosemirrorJSONToYDoc(schema, VERSION, 'default')
const versionB64 = toBase64(Y.encodeStateAsUpdate(versionDoc))

const payload = (body: object, onlyIfEmpty: boolean) => JSON.stringify({ ...body, onlyIfEmpty })

// `restoreVersion` also returns whether it applied (native shows a failure).
type Run = (editor: Editor) => boolean | void
// Each command on an entry with a body: `null` expects the Y.Doc untouched,
// a document expects the fragment to become exactly it.
const cases: [name: string, run: Run, expected: JSONContent | null][] = [
  [
    'onlyIfEmpty on a non-empty entry is ignored',
    (e) => setContentCommand(e, payload({ kind: 'html', html: '<h2>Today</h2>' }, true)),
    null,
  ],
  [
    'a doc the schema cannot hold is refused whole',
    (e) => {
      const doc = { type: 'doc', content: [{ type: 'notANode' }] }
      setContentCommand(e, payload({ kind: 'doc', doc }, false))
    },
    null,
  ],
  [
    'a blank html body is ignored',
    (e) => setContentCommand(e, payload({ kind: 'html', html: ' \n ' }, false)),
    null,
  ],
  [
    'restoreVersion of bytes that are not a Yjs update is ignored',
    (e) => restoreVersionCommand(e, toBase64(new TextEncoder().encode('not a yjs update'))),
    null,
  ],
  [
    'restoreVersion of a version with an empty document is refused',
    (e) => restoreVersionCommand(e, toBase64(Y.encodeStateAsUpdate(new Y.Doc()))),
    null,
  ],
  // Positive controls: the harness sees a write when one is allowed.
  [
    'a valid doc replaces the body',
    (e) => setContentCommand(e, payload({ kind: 'doc', doc: VERSION }, false)),
    VERSION,
  ],
  ['restoreVersion reproduces the version', (e) => restoreVersionCommand(e, versionB64), VERSION],
]

describe('editor commands', () => {
  it.each(cases)('%s', async (_name, run, expected) => {
    const doc = prosemirrorJSONToYDoc(schema, ENTRY, 'default')
    const element = document.createElement('div')
    document.body.appendChild(element)
    const editor = new Editor({
      element,
      extensions: [
        ...buildMobileExtensions(),
        Collaboration.configure({ document: doc, field: 'default' }),
      ],
    })
    editors.push(editor)
    // y-prosemirror renders the Y.Doc into the editor in a macrotask: until
    // then `editor.isEmpty` would describe the blank initial document.
    await settle()
    expect(editor.isEmpty).toBe(false)

    const before = yDocToProsemirrorJSON(doc, 'default')
    const shown = editor.getJSON()
    const stateVector = Y.encodeStateVector(doc)
    const updates: Uint8Array[] = []
    doc.on('update', (update: Uint8Array) => updates.push(update))

    const result = run(editor)
    await settle()

    if (typeof result === 'boolean') expect(result).toBe(expected !== null)

    if (expected === null) {
      expect(updates).toHaveLength(0)
      expect(Y.encodeStateVector(doc)).toEqual(stateVector)
      expect(yDocToProsemirrorJSON(doc, 'default')).toEqual(before)
      // y-prosemirror holds back a document equal to the empty one until the
      // first local edit, so a wipe shows in the editor before the Y.Doc.
      expect(editor.getJSON()).toEqual(shown)
    } else {
      expect(updates.length).toBeGreaterThan(0)
      expect(yDocToProsemirrorJSON(doc, 'default')).toEqual(expected)
    }
  })
})
