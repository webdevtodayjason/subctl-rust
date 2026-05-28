// Composer — a multi-line text input matched against the spec's input
// requirements for Tier 1:
//
//   - Enter submits the buffer
//   - Alt+Enter / Shift+Enter / Ctrl+Enter / \-Enter insert a newline
//   - Esc / Ctrl+C exits (Ctrl+C interrupts a streaming turn first)
//   - Ctrl+W deletes the previous word (Tier 2 reaches further; this is
//     a minimum useful editor)
//   - Backspace + Delete + arrow nav for in-line edits
//   - Up/Down cycle input history when caret is at start/end-of-line
//
// We intentionally avoid the 1300-line Hermes textInput.tsx port for v0.1.
// The user is willing to live with a competent stock editor while we ship
// the chat surface; full grapheme-aware editing comes later.

import { Box, Text, useInput } from 'ink'
import React, { useCallback, useEffect, useRef, useState } from 'react'

import type { Theme } from '../theme.js'

interface ComposerProps {
  theme: Theme
  /** Disabled while the assistant is streaming (Ctrl+C still routed). */
  disabled: boolean
  /** Submission callback. Receives the trimmed buffer. */
  onSubmit: (text: string) => void
  /** Ctrl+C handler — caller decides what 'interrupt' means. */
  onInterrupt: () => void
  /** Esc handler — exit the TUI when buffer is empty, clear otherwise. */
  onExit: () => void
  /** Optional placeholder shown when buffer is empty. */
  placeholder?: string
  /** Prompt history for up/down cycling. */
  history: readonly string[]
}

export function Composer(props: ComposerProps): React.ReactElement {
  const {
    theme,
    disabled,
    onSubmit,
    onInterrupt,
    onExit,
    placeholder,
    history
  } = props

  const [buf, setBuf] = useState('')
  const [cursor, setCursor] = useState(0)
  const histIdxRef = useRef<number | null>(null)
  const savedRef = useRef<string>('')

  useEffect(() => {
    // When `disabled` flips on (streaming starts), preserve current buffer
    // so the user can keep editing while tokens flow back. No reset.
  }, [disabled])

  const insert = useCallback(
    (text: string) => {
      setBuf((prev) => prev.slice(0, cursor) + text + prev.slice(cursor))
      setCursor((c) => c + text.length)
    },
    [cursor]
  )

  const backspace = useCallback(() => {
    if (cursor === 0) {
      return
    }
    setBuf((prev) => prev.slice(0, cursor - 1) + prev.slice(cursor))
    setCursor((c) => Math.max(0, c - 1))
  }, [cursor])

  const deleteRight = useCallback(() => {
    setBuf((prev) =>
      cursor >= prev.length ? prev : prev.slice(0, cursor) + prev.slice(cursor + 1)
    )
  }, [cursor])

  const wordBackspace = useCallback(() => {
    setBuf((prev) => {
      if (cursor === 0) {
        return prev
      }
      let start = cursor
      while (start > 0 && /\s/.test(prev[start - 1] ?? '')) {
        start--
      }
      while (start > 0 && !/\s/.test(prev[start - 1] ?? '')) {
        start--
      }
      setCursor(start)
      return prev.slice(0, start) + prev.slice(cursor)
    })
  }, [cursor])

  const submit = useCallback(() => {
    const text = buf.trim()
    if (text === '') {
      return
    }
    setBuf('')
    setCursor(0)
    histIdxRef.current = null
    savedRef.current = ''
    onSubmit(text)
  }, [buf, onSubmit])

  useInput(
    (input, key) => {
      // Ctrl+C — routed to caller regardless of disabled state.
      if (key.ctrl && input === 'c') {
        if (disabled) {
          onInterrupt()
        } else if (buf !== '') {
          setBuf('')
          setCursor(0)
        } else {
          onExit()
        }
        return
      }
      if (key.escape) {
        onExit()
        return
      }

      // Newline modifiers — Alt/Meta/Shift/Ctrl + Enter.
      // Ink's `key.return` is plain Enter; the multi-modifier flags are
      // available as `key.shift` etc. on supported terminals.
      if (key.return) {
        if (key.shift || key.meta || (key.ctrl && !disabled)) {
          // Alt-Enter / Shift-Enter inserts a newline; modifier-Enter on
          // some terminals (Ctrl) only fires when not streaming so we
          // don't confuse it with a Ctrl-C-during-stream.
          insert('\n')
          return
        }
        // Backslash-Enter fallback — if the last char is `\`, swap it
        // for a newline so terminals that swallow modifier-Enter still
        // get multi-line continuation.
        if (buf.endsWith('\\') && cursor === buf.length) {
          setBuf((prev) => prev.slice(0, -1) + '\n')
          setCursor((c) => c) // \ removed, \n added → same length
          return
        }
        // Don't fire a second turn while one is in flight — Enter is
        // suppressed during streaming so the operator's draft stays in
        // the buffer for after the current reply finishes.  Ctrl+C is
        // the documented escape hatch (placeholder advertises it).
        if (disabled) {
          return
        }
        submit()
        return
      }

      if (key.backspace || key.delete) {
        // Some terminals send DEL (\x7f) as key.delete for backspace.
        // We treat both as backspace; right-delete is keyed via key.delete
        // with `input === ''` on a forward-delete key — rare in practice,
        // ignored for v0.1.
        backspace()
        return
      }

      if (key.ctrl && input === 'w') {
        wordBackspace()
        return
      }
      if (key.ctrl && input === 'd') {
        // Empty buffer + Ctrl+D → exit (matches Hermes Cmd/Ctrl-D
        // convention).  Non-empty buffer + Ctrl+D → forward-delete.
        if (buf === '') {
          onExit()
        } else {
          deleteRight()
        }
        return
      }
      if (key.ctrl && input === 'u') {
        // Kill from cursor to start of line.
        setBuf((prev) => prev.slice(cursor))
        setCursor(0)
        return
      }
      if (key.ctrl && input === 'k') {
        // Kill from cursor to end of line.
        setBuf((prev) => prev.slice(0, cursor))
        return
      }
      if (key.ctrl && input === 'a') {
        setCursor(0)
        return
      }
      if (key.ctrl && input === 'e') {
        setCursor(buf.length)
        return
      }

      if (key.leftArrow) {
        setCursor((c) => Math.max(0, c - 1))
        return
      }
      if (key.rightArrow) {
        setCursor((c) => Math.min(buf.length, c + 1))
        return
      }
      if (key.upArrow) {
        cycleHistory(-1)
        return
      }
      if (key.downArrow) {
        cycleHistory(+1)
        return
      }

      // Plain printable input — ink filters out control bytes for us.
      if (input && !key.ctrl && !key.meta) {
        insert(input)
      }
    },
    { isActive: true }
  )

  function cycleHistory(dir: -1 | 1): void {
    if (history.length === 0) {
      return
    }
    let idx = histIdxRef.current
    if (idx === null) {
      if (dir === -1) {
        idx = history.length - 1
        savedRef.current = buf
      } else {
        return
      }
    } else {
      idx = idx + dir
      if (idx < 0) {
        idx = 0
      }
      if (idx >= history.length) {
        // Step past most-recent → restore in-flight draft.
        histIdxRef.current = null
        setBuf(savedRef.current)
        setCursor(savedRef.current.length)
        return
      }
    }
    histIdxRef.current = idx
    const v = history[idx] ?? ''
    setBuf(v)
    setCursor(v.length)
  }

  const showPlaceholder = buf === '' && (placeholder ?? '') !== ''

  return (
    <Box flexDirection="column">
      {buf.includes('\n') ? (
        <Text color={theme.color.text}>{buf}</Text>
      ) : (
        <Box>
          {showPlaceholder ? (
            <Text color={theme.color.muted} dimColor>
              {placeholder}
            </Text>
          ) : (
            <Text color={theme.color.text}>{renderWithCursor(buf, cursor)}</Text>
          )}
        </Box>
      )}
    </Box>
  )
}

/**
 * Insert a visible block-cursor caret at `cursor` so the operator can
 * see where the next character will land. Ink's hardware cursor is
 * unreliable across terminals; the inverse-glyph trick is portable.
 */
function renderWithCursor(buf: string, cursor: number): string {
  if (buf === '') {
    return '▍'
  }
  const safeCursor = Math.min(cursor, buf.length)
  if (safeCursor >= buf.length) {
    return buf + '▍'
  }
  return buf.slice(0, safeCursor) + '▍' + buf.slice(safeCursor + 1)
}
