'use dom'

import { Editor, getSchema, type JSONContent } from '@tiptap/core'
import Collaboration from '@tiptap/extension-collaboration'
import { yDocToProsemirrorJSON } from '@tiptap/y-tiptap'
import { useDOMImperativeHandle, type DOMProps } from 'expo/dom'
import { useEffect, useRef, useState, type Ref } from 'react'
import * as Y from 'yjs'

import desktopSchema from './__fixtures__/desktop-schema.json'
import { installContentPolicy, safeExternalUrl, setLinkHandler } from './contentPolicy'
import { fromBase64, restoreVersionCommand, setContentCommand } from './editorCommands'
import { buildMobileExtensions } from './extensions'
import { schemaDifferences, snapshotSchema } from './schemaSnapshot'

/** Desktop `AUTO_SAVE_DEBOUNCE_MS` (EditorPanel.tsx). */
const SAVE_DEBOUNCE_MS = 1500
/** Desktop `previewText = contentText.slice(0, 150)` (EditorPanel.tsx). */
const PREVIEW_LENGTH = 150
/** At most one `onFormatState` per this interval while typing or moving the caret. */
const FORMAT_STATE_THROTTLE_MS = 100
/** At most one `onActivity` per this interval while the document changes. */
const ACTIVITY_THROTTLE_MS = 10_000

// Before the editor mounts and renders any media node (see contentPolicy.ts).
installContentPolicy()

/**
 * The WebView has no CSS reset: without this, the body's 8 px margin and the
 * first paragraph's top margin put the first line below the native
 * placeholder's (padding 12, then 17 px / 1.5 text).
 */
const EDITOR_CSS = 'body { margin: 0 } .ProseMirror > :first-child { margin-top: 0 }'

/** What the native toolbar shows as active. */
export type Format =
  | 'bold'
  | 'italic'
  | 'strike'
  | 'highlight'
  | 'heading'
  | 'bulletList'
  | 'orderedList'
  | 'taskList'
  | 'blockquote'
  | 'link'

export type FormatState = {
  /** `heading` is level 2 only, the level the toolbar toggles. */
  active: Record<Format, boolean>
  focused: boolean
  /** The link under the caret, to prefill the link prompt; `null` when none. */
  linkHref: string | null
}

export type EditorCommand =
  | Exclude<Format, 'link'>
  | 'setLink'
  | 'focusEnd'
  | 'blur'
  | 'setContent'
  | 'restoreVersion'

/**
 * Native calls `flush()`; the DOM side answers with `onFlushed` once the save
 * is done. `exec` runs a toolbar command (a no-op unless editable); `arg` is
 * the URL for `setLink`, where '' removes the link, a JSON
 * `SetContentPayload` (templateBody.ts) for `setContent`, and a version's Yjs
 * state (base64) for `restoreVersion`, which answers with `onRestored`.
 * Invalid arguments do nothing.
 */
export type EntryEditorHandle = {
  flush: () => void
  exec(cmd: EditorCommand, arg?: string): void
}

/**
 * The same handle as the DOM side receives it: any JSON value from native,
 * checked before use. Both `exec`s use method syntax (bivariant parameters),
 * so the screen's `Ref<EntryEditorHandle>` still fits.
 */
type BridgeHandle = {
  flush: () => void
  exec(cmd?: unknown, arg?: unknown): void
}

interface Props {
  /** The entry's Yjs full state (base64); '' for an entry with no body yet. */
  initialStateB64: string
  /**
   * Debounced save: the FULL state (`Y.encodeStateAsUpdate`, base64), never an
   * incremental update, plus the search text and the preview, computed like
   * the desktop editor.
   */
  onUpdate: (fullStateB64: string, contentText: string, previewText: string) => Promise<void>
  /** Called after each `flush()`: any pending save has completed (or failed). */
  onFlushed: () => Promise<void>
  /**
   * Once, when the document is in the DOM: native can drop its placeholder.
   * `editable` is false when the entry opened read-only (asked for, or the
   * unsafe fallback), so native can lock its own fields too.
   */
  onReady: (editable: boolean) => Promise<void>
  /** Throttled, and only when it changed; editable mode only. */
  onFormatState: (state: FormatState) => Promise<void>
  /** A tap on a link, only for an absolute http(s) or mailto URL; nothing navigates here. */
  onLinkPress: (url: string) => Promise<void>
  /**
   * After each `restoreVersion` in editable mode: false when it was refused
   * (bytes that do not decode, an empty or invalid document); the entry is
   * then unchanged. Read-only modes never answer: native checks `editable`.
   */
  onRestored: (ok: boolean) => Promise<void>
  /**
   * The document changed (editable mode): at once, then at most every 10 s,
   * so typing with no pause still counts as activity for the second lock's
   * idle re-lock (the save runs only after a pause).
   */
  onActivity?: () => Promise<void>
  readOnly: boolean
  ref?: Ref<EntryEditorHandle>
  dom?: DOMProps
}

type Mode = 'editable' | 'read-only' | 'unsafe'

/** The whole WebView is the writing surface: a tap anywhere below the text focuses it. */
const EDITOR_PROPS = {
  attributes: { style: 'min-height: 80vh; outline: none; font-size: 17px; line-height: 1.5' },
}

/**
 * The entry editor in a WebView. The Y.Doc lives here; only the full state
 * crosses to native. Safety contract 4: opening never mutates the document,
 * and a document that cannot be loaded safely (schema differs from desktop,
 * bytes do not decode) is shown read-only WITHOUT binding Collaboration,
 * because the binding itself deletes nodes the schema lacks.
 */
export default function EntryEditorDom({
  initialStateB64,
  onUpdate,
  onFlushed,
  onReady,
  onFormatState,
  onLinkPress,
  onRestored,
  onActivity,
  readOnly,
  ref,
}: Props) {
  const mountRef = useRef<HTMLDivElement>(null)
  const onUpdateRef = useRef(onUpdate)
  onUpdateRef.current = onUpdate
  const onFlushedRef = useRef(onFlushed)
  onFlushedRef.current = onFlushed
  const onReadyRef = useRef(onReady)
  onReadyRef.current = onReady
  const onFormatStateRef = useRef(onFormatState)
  onFormatStateRef.current = onFormatState
  const onLinkPressRef = useRef(onLinkPress)
  onLinkPressRef.current = onLinkPress
  const onRestoredRef = useRef(onRestored)
  onRestoredRef.current = onRestored
  const onActivityRef = useRef(onActivity)
  onActivityRef.current = onActivity
  /** Saves now what the debounce would save later; a no-op when read-only. */
  const flushRef = useRef<() => Promise<void>>(() => Promise.resolve())
  /** Runs a toolbar command; a no-op until the editable editor exists. */
  const execRef = useRef<(cmd: EditorCommand, arg: string) => void>(() => {})
  const [mode, setMode] = useState<Mode | null>(null)

  // Expo's DOM root never unmounts: when native removes this component (Back,
  // another entry) the page is just torn down and no React cleanup runs. So
  // the screen calls `flush()` and waits for `onFlushed` before leaving.
  useDOMImperativeHandle<BridgeHandle>(
    ref ?? null,
    () => ({
      flush: () => {
        void flushRef.current().finally(() => void onFlushedRef.current())
      },
      exec: (cmd, arg) => {
        if (isEditorCommand(cmd)) execRef.current(cmd, typeof arg === 'string' ? arg : '')
      },
    }),
    [],
  )

  // Links are tappable in every mode, the read-only fallback included.
  useEffect(() => {
    setLinkHandler((url) => void onLinkPressRef.current(url))
    return () => setLinkHandler(null)
  }, [])

  // Mounted once per entry: the screen keys this component by entry id.
  useEffect(() => {
    const element = mountRef.current
    if (!element) return
    // Tiptap emits `create` from a timer after it builds the view, and y-tiptap
    // renders the document synchronously while the view is built: so this runs
    // once, with the text in the DOM (never if the editor is destroyed first).
    // Not `requestAnimationFrame`: WebKit stalls it ~10 s in an empty entry.
    const onCreate = (editable: boolean) => () => void onReadyRef.current(editable)
    const doc = new Y.Doc()
    let safe = schemaDifferences(desktopSchemaSnapshot(), mobileSchemaSnapshot()).length === 0
    try {
      if (initialStateB64) Y.applyUpdate(doc, fromBase64(initialStateB64))
    } catch {
      safe = false
    }

    if (!safe) {
      setMode('unsafe')
      const editor = new Editor({
        element,
        editable: false,
        editorProps: EDITOR_PROPS,
        extensions: buildMobileExtensions(),
        content: readOnlyContent(doc),
        onCreate: onCreate(false),
      })
      return () => {
        editor.destroy()
        doc.destroy()
      }
    }

    setMode(readOnly ? 'read-only' : 'editable')
    const editor = new Editor({
      element,
      editable: !readOnly,
      editorProps: EDITOR_PROPS,
      extensions: [
        ...buildMobileExtensions(),
        Collaboration.configure({ document: doc, field: 'default' }),
      ],
      onCreate: onCreate(!readOnly),
    })
    if (readOnly) {
      return () => {
        editor.destroy()
        doc.destroy()
      }
    }

    // Like the desktop's skip of the initial Collaboration sync: save only
    // when the document really differs from what was last stored.
    let lastSaved = Y.snapshot(doc)
    let timer: ReturnType<typeof setTimeout> | null = null
    let inFlight: Promise<void> = Promise.resolve()
    // True while a save is on its way to native (lastSaved already moved on).
    let saving = false
    const save = () => {
      if (timer) clearTimeout(timer)
      timer = null
      const current = Y.snapshot(doc)
      if (Y.equalSnapshots(lastSaved, current)) return
      lastSaved = current
      saving = true
      const contentText = extractPlainText(doc)
      inFlight = onUpdateRef
        .current(
          toBase64(Y.encodeStateAsUpdate(doc)),
          contentText,
          contentText.slice(0, PREVIEW_LENGTH),
        )
        .catch(() => {
          // Retry on the next edit or flush: the stored doc is unchanged.
          lastSaved = Y.snapshot(new Y.Doc())
        })
        .finally(() => {
          saving = false
        })
    }
    flushRef.current = async () => {
      await inFlight
      save()
      await inFlight
    }
    // Leading-edge throttle for `onActivity`.
    let lastActivity = 0
    const onDocUpdate = () => {
      if (timer) clearTimeout(timer)
      timer = setTimeout(save, SAVE_DEBOUNCE_MS)
      const now = Date.now()
      if (now - lastActivity >= ACTIVITY_THROTTLE_MS) {
        lastActivity = now
        void onActivityRef.current?.()
      }
    }
    // Backgrounding the app: start the save now. It cannot be awaited, so
    // a kill right after may still lose it.
    const onHidden = () => {
      if (document.visibilityState === 'hidden') save()
    }
    doc.on('update', onDocUpdate)
    document.addEventListener('visibilitychange', onHidden)
    window.addEventListener('pagehide', save)

    // Trailing throttle; an unchanged state does not cross the bridge again.
    let formatTimer: ReturnType<typeof setTimeout> | null = null
    let lastFormat = ''
    const sendFormatState = () => {
      formatTimer = null
      if (editor.isDestroyed) return
      const state = formatState(editor)
      const json = JSON.stringify(state)
      if (json === lastFormat) return
      lastFormat = json
      void onFormatStateRef.current(state)
    }
    const onFormatChange = () => {
      formatTimer ??= setTimeout(sendFormatState, FORMAT_STATE_THROTTLE_MS)
    }
    editor.on('transaction', onFormatChange)
    editor.on('focus', onFormatChange)
    editor.on('blur', onFormatChange)
    execRef.current = (cmd, arg) => {
      if (editor.isDestroyed) return
      if (cmd === 'restoreVersion') {
        // Refuse while anything is unsaved (typed after native's flush, a save
        // still on its way, or a failed save): the pre-restore snapshot holds
        // only the stored doc, so replacing unsaved text would leave it in no
        // version.
        const unsaved = saving || !Y.equalSnapshots(lastSaved, Y.snapshot(doc))
        void onRestoredRef.current(!unsaved && restoreVersionCommand(editor, arg))
      } else COMMANDS[cmd](editor, arg)
    }

    return () => {
      // Only runs on an in-page unmount, never on native teardown (see flush).
      doc.off('update', onDocUpdate)
      document.removeEventListener('visibilitychange', onHidden)
      window.removeEventListener('pagehide', save)
      if (formatTimer) clearTimeout(formatTimer)
      execRef.current = () => {}
      flushRef.current = () => Promise.resolve()
      save()
      editor.destroy()
      doc.destroy()
    }
    // One document per mount: later prop changes are ignored on purpose.
  }, [])

  return (
    // Expo's DOM root (`#root`) is a row flexbox, which shrinks this wrapper
    // to its content: an empty entry's editor was 0 px wide, so a tap could
    // never land on it. `flex: 1` makes the writing surface the full width.
    <div
      style={{
        flex: 1,
        minWidth: 0,
        fontFamily: '-apple-system, system-ui, sans-serif',
        padding: 12,
      }}
    >
      <style>{EDITOR_CSS}</style>
      {mode === 'unsafe' ? (
        <p data-testid="editor-read-only-fallback">
          This entry opens read-only on mobile.
        </p>
      ) : null}
      <div ref={mountRef} data-mode={mode ?? 'loading'} />
    </div>
  )
}

function formatState(editor: Editor): FormatState {
  const href: unknown = editor.getAttributes('link').href
  return {
    active: {
      bold: editor.isActive('bold'),
      italic: editor.isActive('italic'),
      strike: editor.isActive('strike'),
      highlight: editor.isActive('highlight'),
      heading: editor.isActive('heading', { level: 2 }),
      bulletList: editor.isActive('bulletList'),
      orderedList: editor.isActive('orderedList'),
      taskList: editor.isActive('taskList'),
      blockquote: editor.isActive('blockquote'),
      link: editor.isActive('link'),
    },
    focused: editor.isFocused,
    linkHref: typeof href === 'string' && href !== '' ? href : null,
  }
}

/** The toolbar's commands, run as the desktop bubble menu runs them. */
const COMMANDS: Record<EditorCommand, (editor: Editor, arg: string) => void> = {
  bold: (editor) => editor.chain().focus().toggleBold().run(),
  italic: (editor) => editor.chain().focus().toggleItalic().run(),
  strike: (editor) => editor.chain().focus().toggleStrike().run(),
  highlight: (editor) => editor.chain().focus().toggleHighlight().run(),
  heading: (editor) => editor.chain().focus().toggleHeading({ level: 2 }).run(),
  bulletList: (editor) => editor.chain().focus().toggleBulletList().run(),
  orderedList: (editor) => editor.chain().focus().toggleOrderedList().run(),
  taskList: (editor) => editor.chain().focus().toggleTaskList().run(),
  blockquote: (editor) => editor.chain().focus().toggleBlockquote().run(),
  setLink: (editor, arg) => {
    const url = arg.trim()
    const chain = editor.chain().focus().extendMarkRange('link')
    // `unsetLink`, not desktop's `setLink({ href: '' })`, which keeps an empty link mark.
    if (url === '') chain.unsetLink().run()
    // Only a URL a tap can open (desktop's rule without `tel:`); anything else is ignored.
    else {
      const href = safeExternalUrl(url)
      if (href) chain.setLink({ href }).run()
    }
  },
  // Tiptap's `focus` and `blur` finish in a `requestAnimationFrame`, which
  // WebKit stalls ~10 s in an empty entry: that part runs here, synchronously.
  focusEnd: (editor) => {
    editor.commands.focus('end')
    editor.view.focus()
  },
  // Not `commands.blur()`: it also clears the selection, which Done keeps.
  blur: (editor) => editor.view.dom.blur(),
  setContent: setContentCommand,
  restoreVersion: restoreVersionCommand,
}

/** Native may send anything across the bridge; unknown commands do nothing. */
function isEditorCommand(cmd: unknown): cmd is EditorCommand {
  return typeof cmd === 'string' && Object.hasOwn(COMMANDS, cmd)
}

let desktopSnapshotCache: unknown
function desktopSchemaSnapshot(): unknown {
  desktopSnapshotCache ??= desktopSchema.schema
  return desktopSnapshotCache
}

function mobileSchemaSnapshot(): unknown {
  return snapshotSchema(getSchema(buildMobileExtensions()))
}

/** Best-effort content for the read-only fallback, from a copy of the doc. */
function readOnlyContent(doc: Y.Doc): JSONContent | undefined {
  try {
    const copy = new Y.Doc()
    Y.applyUpdate(copy, Y.encodeStateAsUpdate(doc))
    return yDocToProsemirrorJSON(copy, 'default') as JSONContent
  } catch {
    return undefined
  }
}

// --- Desktop `extractPlainText` (src/lib/yjs.ts), copied verbatim ---------
// Including its quirk: the tag is lowercased before the camelCase set is
// checked, so only paragraph/heading/blockquote end with a newline. The Rust
// merge (`yjs_doc::extract_entry_text`) mirrors the same quirk.

function extractPlainText(doc: Y.Doc): string {
  const fragment = doc.getXmlFragment('default')
  return xmlFragmentToText(fragment).trim()
}

function xmlFragmentToText(fragment: Y.XmlFragment): string {
  let result = ''
  for (let i = 0; i < fragment.length; i++) {
    const child = fragment.get(i)
    if (child instanceof Y.XmlText) {
      result += child.toString()
    } else if (child instanceof Y.XmlElement) {
      result += xmlElementToText(child)
    }
  }
  return result
}

const BLOCK_TAGS = new Set([
  'paragraph',
  'heading',
  'listItem',
  'taskItem',
  'blockquote',
  'codeBlock',
  'bulletList',
  'orderedList',
  'taskList',
  'tableRow',
  'tableCell',
  'tableHeader',
  'horizontalRule',
])

function xmlElementToText(elem: Y.XmlElement): string {
  if (elem.nodeName?.toLowerCase() === 'mention') {
    return elem.getAttribute('label') ?? ''
  }
  let result = ''
  for (let i = 0; i < elem.length; i++) {
    const child = elem.get(i)
    if (child instanceof Y.XmlText) {
      result += child.toString()
    } else if (child instanceof Y.XmlElement) {
      result += xmlElementToText(child)
    }
  }
  const tag = elem.nodeName?.toLowerCase() ?? ''
  return BLOCK_TAGS.has(tag) ? result + '\n' : result
}

// --- base64 (no Uint8Array.toBase64 on the iOS 17 WebView) ----------------

function toBase64(bytes: Uint8Array): string {
  let binary = ''
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000))
  }
  return btoa(binary)
}
