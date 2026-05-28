// SSE chunk parser tests. The wire format the v4 chat handler emits is
// well-defined (`crates/evy-comms/src/chat.rs:214`) so these fixtures are
// the canonical shape — keep in sync if the Rust side ever changes.

import { describe, test, expect } from 'vitest'

import { SseChunkParser } from '../src/gateway/sse.js'
import type { ChatStreamEvent } from '../src/gateway/types.js'

function parseFull(chunks: readonly string[]): ChatStreamEvent[] {
  const parser = new SseChunkParser()
  const events: ChatStreamEvent[] = []
  for (const c of chunks) {
    for (const ev of parser.push(c)) {
      events.push(ev)
    }
  }
  for (const ev of parser.flush()) {
    events.push(ev)
  }
  return events
}

describe('SseChunkParser', () => {
  test('parses a happy-path token + done sequence', () => {
    const wire =
      'data: {"kind":"token","content":"Hello"}\n\n' +
      'data: {"kind":"token","content":", "}\n\n' +
      'data: {"kind":"token","content":"world"}\n\n' +
      'data: {"kind":"done","session_id":"abc-123"}\n\n'
    const evs = parseFull([wire])
    expect(evs).toHaveLength(4)
    expect(evs[0]).toEqual({ kind: 'token', content: 'Hello' })
    expect(evs[3]).toEqual({ kind: 'done', session_id: 'abc-123' })
  })

  test('reassembles events split across multiple chunks', () => {
    const wire = 'data: {"kind":"token","content":"H"}\n\n'
    // Split mid-field across 3 chunks
    const evs = parseFull([wire.slice(0, 7), wire.slice(7, 25), wire.slice(25)])
    expect(evs).toEqual([{ kind: 'token', content: 'H' }])
  })

  test('handles CRLF line terminators', () => {
    const wire = 'data: {"kind":"token","content":"x"}\r\n\r\n'
    expect(parseFull([wire])).toEqual([{ kind: 'token', content: 'x' }])
  })

  test('handles skill_loaded events', () => {
    const wire = 'data: {"kind":"skill_loaded","name":"plan"}\n\n'
    expect(parseFull([wire])).toEqual([
      { kind: 'skill_loaded', name: 'plan' }
    ])
  })

  test('handles error termination', () => {
    const wire =
      'data: {"kind":"error","error_kind":"backend","message":"upstream 502"}\n\n'
    expect(parseFull([wire])).toEqual([
      { kind: 'error', error_kind: 'backend', message: 'upstream 502' }
    ])
  })

  test('multi-line data field is joined with \\n', () => {
    const wire = 'data: {"kind":"token",\ndata: "content":"x"}\n\n'
    expect(parseFull([wire])).toEqual([{ kind: 'token', content: 'x' }])
  })

  test('ignores comments and unknown fields', () => {
    const wire =
      ': keepalive\nevent: ignored\nid: 7\n' +
      'data: {"kind":"token","content":"x"}\n\n'
    expect(parseFull([wire])).toEqual([{ kind: 'token', content: 'x' }])
  })

  test('malformed JSON surfaces as an error event', () => {
    const wire = 'data: {bogus\n\n'
    const evs = parseFull([wire])
    expect(evs).toHaveLength(1)
    expect(evs[0]?.kind).toBe('error')
  })

  test('empty input yields no events', () => {
    expect(parseFull([])).toEqual([])
    expect(parseFull([''])).toEqual([])
  })

  test('byte-by-byte feed still works', () => {
    const wire = 'data: {"kind":"done","session_id":"u"}\n\n'
    const chunks = wire.split('').map((c) => c)
    const evs = parseFull(chunks)
    expect(evs).toEqual([{ kind: 'done', session_id: 'u' }])
  })
})
