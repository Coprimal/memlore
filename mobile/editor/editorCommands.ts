import type { Editor, JSONContent } from '@tiptap/core'
import { yDocToProsemirrorJSON } from '@tiptap/y-tiptap'
import * as Y from 'yjs'

import type { SetContentPayload } from './templateBody'

/**
 * The two editor commands that replace the whole document, and their guards
 * against wiping it. Used by `EntryEditorDom.tsx`; plain so tests can bind them.
 */

/**
 * A template body (desktop Editor.tsx, without its `focus()`, which would
 * raise the keyboard). Through Collaboration, so it saves like typing.
 */
export function setContentCommand(editor: Editor, arg: string): void {
  const payload = parseSetContentPayload(arg)
  if (!payload) return
  if (payload.onlyIfEmpty && !editor.isEmpty) return
  if (payload.kind === 'html') {
    // Lenient like desktop: the schema drops what it cannot hold.
    editor.commands.setContent(payload.html, { emitUpdate: true })
  } else {
    replaceWithJson(editor, payload.doc)
  }
}

/**
 * Desktop EditorPanel's restore: the version's document (its Yjs state,
 * base64) replaces the current one as an edit, so other devices' changes
 * merge, not clobber.
 */
export function restoreVersionCommand(editor: Editor, arg: string): void {
  const tmp = new Y.Doc()
  try {
    Y.applyUpdate(tmp, fromBase64(arg))
    replaceWithJson(editor, yDocToProsemirrorJSON(tmp, 'default') as JSONContent)
  } catch {
    // Bytes that do not decode: the entry stays as it is.
  } finally {
    tmp.destroy()
  }
}

/**
 * `setContent` of a JSON document, refused whole when the schema cannot hold
 * it. Without `errorOnInvalidContent`, tiptap swaps invalid JSON for an empty
 * document and the entry would be wiped. It throws before any dispatch.
 */
function replaceWithJson(editor: Editor, doc: JSONContent): void {
  try {
    editor.commands.setContent(doc, { emitUpdate: true, errorOnInvalidContent: true })
  } catch {
    // Not a document of this schema: nothing is written.
  }
}

/** The `setContent` argument, checked: it comes from native as any string. */
function parseSetContentPayload(arg: string): SetContentPayload | null {
  let value: unknown
  try {
    value = JSON.parse(arg)
  } catch {
    return null
  }
  if (typeof value !== 'object' || value === null) return null
  const { kind, html, doc, onlyIfEmpty } = value as Record<string, unknown>
  if (typeof onlyIfEmpty !== 'boolean') return null
  // An empty string would parse to an empty document and wipe the entry.
  if (kind === 'html' && typeof html === 'string' && html.trim() !== '') {
    return { kind, html, onlyIfEmpty }
  }
  if (kind === 'doc' && typeof doc === 'object' && doc !== null && !Array.isArray(doc)) {
    return { kind, doc: doc as JSONContent, onlyIfEmpty }
  }
  return null
}

/** base64 to bytes (no `Uint8Array.fromBase64` on the iOS 17 WebView). */
export function fromBase64(b64: string): Uint8Array {
  const binary = atob(b64)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  return bytes
}
