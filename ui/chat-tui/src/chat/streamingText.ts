// Prefix-cache invariant — ported from Hermes
// /Users/sem/code/hermes-agent/ui-tui/src/components/streamingMarkdown.tsx.
//
// In Hermes the *renderer* is a full markdown engine and the *invariant*
// is what keeps re-tokenisation bounded. We are not shipping a markdown
// renderer for v0.1, but the invariant still applies: we split the live
// streaming text at the last stable `\n\n` boundary outside any fenced
// code block, hold the prefix in a ref so it only grows monotonically,
// and render the prefix/suffix as two siblings.
//
// For our v0.1 raw-text path the split is purely cosmetic — `<Text>`
// doesn't re-tokenize. But the function is correct, tested, and ready
// for the moment we plug a markdown renderer in.

/**
 * Count fence toggles up to `end`. If odd, the boundary at `end` would
 * sit inside a fenced code block — splitting there would break rendering.
 */
function fenceOpenAt(s: string, end: number): boolean {
  let codeOpen = false
  let i = 0
  while (i < end) {
    const nl = s.indexOf('\n', i)
    const lineEnd = nl < 0 || nl > end ? end : nl
    const line = s.slice(i, lineEnd).trim()
    if (/^(?:`{3,}|~{3,})/.test(line)) {
      codeOpen = !codeOpen
    }
    if (nl < 0 || nl >= end) {
      break
    }
    i = nl + 1
  }
  return codeOpen
}

/**
 * Return the index AFTER the last stable `\n\n` boundary outside any
 * fenced block. -1 if no safe boundary exists yet (e.g. mid-fence).
 *
 * Mirrors `findStableBoundary` at
 * hermes-agent/ui-tui/src/components/streamingMarkdown.tsx:107-129.
 */
export function findStableBoundary(text: string): number {
  let idx = text.length
  while (idx > 0) {
    const boundary = text.lastIndexOf('\n\n', idx - 1)
    if (boundary < 0) {
      return -1
    }
    const splitAt = boundary + 2
    if (!fenceOpenAt(text, splitAt)) {
      return splitAt
    }
    idx = boundary
  }
  return -1
}

/**
 * Stateful splitter — call from a memoized component with a long-lived ref.
 * Returns the prefix (never retreats across calls) and the suffix relative
 * to the current text.
 */
export interface SplitState {
  /** Cached prefix from the last call. Monotonic. */
  prefix: string
}

export function nextSplit(state: SplitState, text: string): {
  prefix: string
  suffix: string
} {
  // Defensive: if the new text doesn't start with our cached prefix
  // (e.g. session reset mid-turn), drop the cache.
  if (!text.startsWith(state.prefix)) {
    state.prefix = ''
  }
  const boundary = findStableBoundary(text)
  if (boundary > state.prefix.length) {
    state.prefix = text.slice(0, boundary)
  }
  return {
    prefix: state.prefix,
    suffix: text.slice(state.prefix.length)
  }
}
