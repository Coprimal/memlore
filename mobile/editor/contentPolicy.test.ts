import { afterEach, expect, it, vi } from 'vitest'

// React Native's global, read when the policy module evaluates.
vi.hoisted(() => {
  ;(globalThis as { __DEV__?: boolean }).__DEV__ = false
})

import { installContentPolicy, setLinkHandler } from './contentPolicy'

afterEach(() => {
  setLinkHandler(null)
  document.body.replaceChildren()
})

// Entry content is untrusted: a tap on a link must never navigate the
// WebView, and only an absolute http(s)/mailto URL may reach native.
it.each([
  ['https://example.com/a?b=1', 'https://example.com/a?b=1'],
  ['javascript:alert(1)', null],
])('a tap on a link to %s never navigates and reports %s', (href, reported) => {
  installContentPolicy()
  const onLink = vi.fn()
  setLinkHandler(onLink)
  const text = linkText(href)

  const click = new MouseEvent('click', { bubbles: true, cancelable: true })
  text.dispatchEvent(click)

  expect(click.defaultPrevented).toBe(true)
  expect(onLink.mock.calls).toEqual(reported === null ? [] : [[reported]])
})

// WKWebView swallows taps inside a contenteditable, so no click comes: the
// tap is caught at touchend. A scroll (moved touch) must stay a scroll.
it.each([
  ['an https tap', 'https://example.com/', 0, true, ['https://example.com/']],
  ['a scroll', 'https://example.com/', 40, false, []],
  ['a javascript: tap', 'javascript:alert(1)', 0, true, []],
])('%s on a link to %s, moved %i px', (_, href, dy, prevented, reported) => {
  installContentPolicy()
  const onLink = vi.fn()
  setLinkHandler(onLink)
  const text = linkText(href)

  const at = (y: number) => [new Touch({ identifier: 1, target: text, clientX: 5, clientY: y })]
  text.dispatchEvent(new TouchEvent('touchstart', { bubbles: true, touches: at(10) }))
  const end = new TouchEvent('touchend', {
    bubbles: true,
    cancelable: true,
    changedTouches: at(10 + dy),
  })
  text.dispatchEvent(end)
  expect(end.defaultPrevented).toBe(prevented)
  if (prevented) {
    // Should WebKit still synthesize the click, it is not reported twice.
    const click = new MouseEvent('click', { bubbles: true, cancelable: true })
    text.dispatchEvent(click)
    expect(click.defaultPrevented).toBe(true)
  }

  expect(onLink.mock.calls).toEqual(reported.map((url) => [url]))
})

/** ProseMirror renders marks inside the anchor: a tap lands on a child. */
function linkText(href: string): HTMLElement {
  const anchor = document.createElement('a')
  anchor.setAttribute('href', href)
  const text = document.createElement('strong')
  text.textContent = 'link'
  anchor.append(text)
  document.body.append(anchor)
  return text
}
