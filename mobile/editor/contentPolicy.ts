/**
 * Entry content is untrusted (synced from other devices, imported): nothing
 * in it may load from the network (an `<img src="https://…">` would leak the
 * IP and the open time) or navigate the WebView. Installed when the editor
 * module evaluates, so before the editor mounts and renders any media node.
 * Scripts, styles and fonts are left alone: the DOM runtime and KaTeX need them.
 */
const CONTENT_SECURITY_POLICY = [
  "img-src 'self' data: blob:",
  "media-src 'self' data: blob:",
  // Nothing in the editor embeds, posts or rebases; keep a future node from doing so.
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-src 'none'",
  // Dev: Metro's Fast Refresh socket is ws://, which WebKit may not count as 'self'.
  __DEV__ ? "connect-src 'self' ws://localhost:*" : "connect-src 'self'",
].join('; ')

/** Schemes native may open, after the user confirms the URL. */
const EXTERNAL_PROTOCOLS = new Set(['http:', 'https:', 'mailto:'])

/** A touch that moves at most this far and lifts within `TAP_MAX_MS` is a tap. */
const TAP_SLOP_PX = 10
const TAP_MAX_MS = 500
/** A click this soon after a handled tap is that tap's synthetic click: not reported again. */
const TAP_CLICK_DEDUPE_MS = 1000

let linkHandler: ((url: string) => void) | null = null
/** The current single-finger touch, while it may still be a tap. */
let tapStart: { x: number; y: number; at: number } | null = null
/** When the last link tap was handled; a click consumes it. */
let lastTapAt: number | null = null

/** Who hears about link taps (the editor's `onLinkPress`); `null` stops reporting. */
export function setLinkHandler(handler: ((url: string) => void) | null) {
  linkHandler = handler
}

/**
 * The normalized URL when `href` is an absolute `http:`, `https:` or
 * `mailto:` URL, otherwise `null` (relative, `javascript:`, `data:`, `file:`…).
 */
export function safeExternalUrl(href: string | null): string | null {
  if (!href) return null
  try {
    const url = new URL(href)
    return EXTERNAL_PROTOCOLS.has(url.protocol) ? url.href : null
  } catch {
    return null
  }
}

export function installContentPolicy() {
  if (typeof document === 'undefined' || document.querySelector('meta[data-memlore-csp]')) return
  const meta = document.createElement('meta')
  meta.httpEquiv = 'Content-Security-Policy'
  meta.content = CONTENT_SECURITY_POLICY
  meta.setAttribute('data-memlore-csp', '')
  document.head.prepend(meta)
  // No long-press "Open link" preview either.
  const style = document.createElement('style')
  style.textContent = 'a { -webkit-touch-callout: none; }'
  document.head.append(style)
  // A tap on a link never navigates, editable or read-only. A safe URL goes
  // to native, which opens it only after the user confirms it.
  document.addEventListener(
    'click',
    (event) => {
      const anchor = linkAt(event.target) ?? linkAtPoint(event.clientX, event.clientY)
      if (!anchor) return
      event.preventDefault()
      const tapped = lastTapAt !== null && Date.now() - lastTapAt <= TAP_CLICK_DEDUPE_MS
      lastTapAt = null
      if (!tapped) reportLink(anchor)
    },
    true,
  )
  // WKWebView's text interaction swallows taps inside a contenteditable (the
  // caret moves, the keyboard or the Paste callout shows): no `click` ever
  // comes. So a tap is caught at `touchend`, whose `preventDefault` also
  // keeps the caret, keyboard and callout down. A moved touch is a scroll.
  // TODO(later): in a focused editor the native callout still shows behind the alert
  // (docs/LATER.md "Mobile M3a notes").
  document.addEventListener(
    'touchstart',
    (event) => {
      const touch = event.touches.length === 1 ? event.touches[0] : undefined
      tapStart = touch ? { x: touch.clientX, y: touch.clientY, at: Date.now() } : null
    },
    { capture: true, passive: true },
  )
  document.addEventListener(
    'touchcancel',
    () => {
      tapStart = null
    },
    true,
  )
  document.addEventListener(
    'touchend',
    (event) => {
      const start = tapStart
      tapStart = null
      const touch = event.changedTouches.length === 1 ? event.changedTouches[0] : undefined
      if (!start || !touch || event.touches.length > 0) return
      // Inside a contenteditable WebKit may target the editing host, not the
      // link under the finger: fall back to what is at the touch point.
      const anchor = linkAt(event.target) ?? linkAtPoint(touch.clientX, touch.clientY)
      if (!anchor) return
      const moved = Math.hypot(touch.clientX - start.x, touch.clientY - start.y)
      if (moved > TAP_SLOP_PX || Date.now() - start.at > TAP_MAX_MS) return
      event.preventDefault()
      lastTapAt = Date.now()
      reportLink(anchor)
    },
    { capture: true, passive: false },
  )
}

function linkAt(target: EventTarget | null): Element | null {
  return target instanceof Element ? target.closest('a') : null
}

function linkAtPoint(x: number, y: number): Element | null {
  return typeof document.elementFromPoint === 'function'
    ? linkAt(document.elementFromPoint(x, y))
    : null
}

/** Only a safe URL reaches native; any other link does nothing. */
function reportLink(anchor: Element) {
  const url = safeExternalUrl(anchor.getAttribute('href'))
  if (url) linkHandler?.(url)
}
