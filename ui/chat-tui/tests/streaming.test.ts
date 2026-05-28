// Tests the prefix-cache invariant — see streamingText.ts.

import { describe, test, expect } from 'vitest'

import { findStableBoundary, nextSplit } from '../src/chat/streamingText.js'

describe('findStableBoundary', () => {
  test('returns -1 when there is no blank line yet', () => {
    expect(findStableBoundary('hello world')).toBe(-1)
    expect(findStableBoundary('one\ntwo')).toBe(-1)
  })

  test('finds the index after the last \\n\\n boundary', () => {
    const txt = 'one\n\ntwo\n\nthree'
    // After the second blank line (between "two" and "three").
    const idx = findStableBoundary(txt)
    expect(idx).toBe('one\n\ntwo\n\n'.length)
    expect(txt.slice(0, idx)).toBe('one\n\ntwo\n\n')
    expect(txt.slice(idx)).toBe('three')
  })

  test('skips boundaries inside fenced code blocks', () => {
    const txt = 'pre\n\n```\ninner\n\nstill inner\n```\n\ntail'
    const idx = findStableBoundary(txt)
    // Must land BEFORE the fenced block (the only outer-fence boundary
    // before "tail" sits AFTER the closing ``` plus blank line).
    const prefix = txt.slice(0, idx)
    expect(prefix).toContain('```\n')
    expect(prefix).toContain('\n```\n\n')
    expect(txt.slice(idx)).toBe('tail')
  })

  test('returns -1 when only fenced-internal boundaries exist', () => {
    const txt = '```\nfirst\n\nsecond'
    expect(findStableBoundary(txt)).toBe(-1)
  })
})

describe('nextSplit', () => {
  test('cache grows monotonically across deltas', () => {
    const state = { prefix: '' }
    expect(nextSplit(state, 'Hel')).toEqual({ prefix: '', suffix: 'Hel' })
    expect(nextSplit(state, 'Hello')).toEqual({
      prefix: '',
      suffix: 'Hello'
    })
    // First boundary appears.
    const r1 = nextSplit(state, 'Hello\n\nworld')
    expect(r1.prefix).toBe('Hello\n\n')
    expect(r1.suffix).toBe('world')
    expect(state.prefix).toBe('Hello\n\n')
    // Boundary doesn't retreat after further suffix-only growth.
    const r2 = nextSplit(state, 'Hello\n\nworld!')
    expect(r2.prefix).toBe('Hello\n\n')
    expect(r2.suffix).toBe('world!')
    // Second boundary advances the prefix.
    const r3 = nextSplit(state, 'Hello\n\nworld!\n\ntail')
    expect(r3.prefix).toBe('Hello\n\nworld!\n\n')
    expect(r3.suffix).toBe('tail')
  })

  test('resets when text no longer starts with prefix', () => {
    const state = { prefix: 'Hello\n\n' }
    const r = nextSplit(state, 'Different')
    expect(r.prefix).toBe('')
    expect(r.suffix).toBe('Different')
  })
})
