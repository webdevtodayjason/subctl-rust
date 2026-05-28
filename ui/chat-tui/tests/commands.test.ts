// Slash command dispatcher tests — mock global fetch and verify the
// system messages each command appends to the transcript. These commands
// are the most likely surface to drift if the daemon shapes change.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { dispatchSlash } from '../src/slash/commands.js'
import { parseSlashCommand } from '../src/slash/parser.js'
import { $transcript, resetTranscript } from '../src/state/transcript.js'

interface FetchCall {
  url: string
  init?: RequestInit
}

function mockFetch(handlers: {
  [pathPrefix: string]:
    | { status: number; body: unknown }
    | ((req: FetchCall) => { status: number; body: unknown } | Promise<{ status: number; body: unknown }>)
}): { calls: FetchCall[]; restore: () => void } {
  const calls: FetchCall[] = []
  const original = globalThis.fetch
  globalThis.fetch = (async (input: unknown, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : (input as URL).toString()
    calls.push({ url, init })
    for (const prefix of Object.keys(handlers)) {
      if (url.includes(prefix)) {
        const handler = handlers[prefix]!
        const result =
          typeof handler === 'function'
            ? await handler({ url, init })
            : handler
        return new Response(JSON.stringify(result.body), {
          status: result.status,
          headers: { 'content-type': 'application/json' }
        })
      }
    }
    return new Response('not found', { status: 404 })
  }) as typeof fetch
  return {
    calls,
    restore: () => {
      globalThis.fetch = original
    }
  }
}

function lastSystemText(): string {
  const msgs = $transcript.get().messages
  for (let i = msgs.length - 1; i >= 0; i--) {
    const m = msgs[i]
    if (m && m.role === 'system') {
      return m.text
    }
  }
  return ''
}

describe('slash commands', () => {
  beforeEach(() => {
    resetTranscript()
  })
  afterEach(() => {
    vi.restoreAllMocks()
  })

  test('/help appends a system message listing commands', async () => {
    const cmd = parseSlashCommand('/help')!
    const outcome = await dispatchSlash(cmd)
    expect(outcome).toEqual({ kind: 'handled' })
    const text = lastSystemText()
    expect(text).toContain('/help')
    expect(text).toContain('/backend')
    expect(text).toContain('/sessions')
  })

  test('/quit returns exit outcome', async () => {
    const outcome = await dispatchSlash(parseSlashCommand('/quit')!)
    expect(outcome).toEqual({ kind: 'exit' })
  })

  test('/q and /exit alias /quit', async () => {
    expect(await dispatchSlash(parseSlashCommand('/q')!)).toEqual({
      kind: 'exit'
    })
    expect(await dispatchSlash(parseSlashCommand('/exit')!)).toEqual({
      kind: 'exit'
    })
  })

  test('/clear resets the transcript', async () => {
    // Seed some history so we can see it disappear.
    $transcript.set({
      messages: [{ id: 1, role: 'user', text: 'hi' }],
      streaming: '',
      isStreaming: false,
      sessionId: 'abc',
      lastError: null,
      liveSkills: []
    })
    await dispatchSlash(parseSlashCommand('/clear')!)
    const state = $transcript.get()
    expect(state.sessionId).toBe(null)
    // /clear resets, then appends the "cleared" system msg.
    expect(state.messages).toHaveLength(1)
    expect(state.messages[0]?.role).toBe('system')
    expect(state.messages[0]?.text).toContain('cleared')
  })

  test('/sessions lists summaries from the daemon', async () => {
    const f = mockFetch({
      '/api/evy/sessions': {
        status: 200,
        body: {
          sessions: [
            {
              id: '11111111-1111-1111-1111-111111111111',
              started_at: '2026-05-28T00:00:00Z',
              last_message_at: '2026-05-28T00:01:00Z',
              message_count: 4,
              preview: 'hello world',
              status: 'active'
            }
          ]
        }
      }
    })
    try {
      await dispatchSlash(parseSlashCommand('/sessions')!)
      expect(f.calls.length).toBe(1)
      expect(f.calls[0]?.url).toContain('/api/evy/sessions')
      const text = lastSystemText()
      expect(text).toContain('11111111')
      expect(text).toContain('hello world')
    } finally {
      f.restore()
    }
  })

  test('/sessions handles empty list', async () => {
    const f = mockFetch({
      '/api/evy/sessions': { status: 200, body: { sessions: [] } }
    })
    try {
      await dispatchSlash(parseSlashCommand('/sessions')!)
      expect(lastSystemText()).toContain('no sessions')
    } finally {
      f.restore()
    }
  })

  test('/skills lists the registry', async () => {
    const f = mockFetch({
      '/api/evy/skills': {
        status: 200,
        body: {
          skills: [
            { name: 'plan', description: 'planning', triggers: [], priority: 0 },
            { name: 'calendar', description: 'cal', triggers: [], priority: 0 }
          ]
        }
      }
    })
    try {
      await dispatchSlash(parseSlashCommand('/skills')!)
      const text = lastSystemText()
      expect(text).toContain('2 registered')
      expect(text).toContain('plan')
      expect(text).toContain('calendar')
    } finally {
      f.restore()
    }
  })

  test('/status surfaces daemon version', async () => {
    const f = mockFetch({
      '/health': { status: 200, body: { ok: true, version: '0.7.0' } }
    })
    try {
      await dispatchSlash(parseSlashCommand('/status')!)
      const text = lastSystemText()
      expect(text).toContain('running')
      expect(text).toContain('0.7.0')
    } finally {
      f.restore()
    }
  })

  test('/status reports failure when daemon unreachable', async () => {
    const f = mockFetch({
      '/health': { status: 503, body: { ok: false } }
    })
    try {
      await dispatchSlash(parseSlashCommand('/status')!)
      expect(lastSystemText()).toContain('failed')
    } finally {
      f.restore()
    }
  })

  test('unknown command returns unknown outcome', async () => {
    const outcome = await dispatchSlash(parseSlashCommand('/bogus')!)
    expect(outcome).toEqual({ kind: 'unknown' })
  })

  test('/backend with no args reports the current setting', async () => {
    // The command reads from CONFIG_TOML_PATH which doesn't exist in tests;
    // verify it surfaces "(unset — daemon default)" rather than crashing.
    await dispatchSlash(parseSlashCommand('/backend')!)
    const text = lastSystemText()
    expect(text).toMatch(/current backend/)
  })

  test('/backend with an unknown value rejects', async () => {
    await dispatchSlash(parseSlashCommand('/backend bogus')!)
    expect(lastSystemText()).toContain('unknown backend')
  })
})
