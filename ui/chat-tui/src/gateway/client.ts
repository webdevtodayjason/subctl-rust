// HTTP client for the v4 Evy daemon.
//
// Uses Node 22's global fetch (undici under the hood). The chat stream
// uses Accept: text/event-stream to opt into the SSE branch; everything
// else is JSON.
//
// Errors map to a Result-shaped return for the SSE iterator (which has to
// surface them inline anyway) and to thrown `GatewayError` for the
// one-shot JSON calls — those have try-once-fail-loud semantics in the
// TUI (and we render the error in the status line).

import { DEFAULT_DAEMON_URL } from '../constants.js'
import { SseChunkParser } from './sse.js'
import type {
  ChatStreamEvent,
  HealthResponse,
  SessionsListResponse,
  SkillsListResponse
} from './types.js'

export class GatewayError extends Error {
  override readonly cause?: unknown
  constructor(message: string, cause?: unknown) {
    super(message)
    this.name = 'GatewayError'
    this.cause = cause
  }
}

/**
 * Open a streaming chat against `POST /api/evy/chat`. Yields each event
 * as it arrives; terminates when the daemon emits `done` or `error`, or
 * when the upstream cancels via the AbortSignal.
 *
 * The caller is responsible for cleanup: pass an AbortSignal from the
 * component that owns the turn, and abort on unmount / Ctrl+C.
 */
export async function* streamChat(opts: {
  url?: string
  sessionId: string | null
  message: string
  signal: AbortSignal
}): AsyncGenerator<ChatStreamEvent, void, void> {
  const url = (opts.url ?? DEFAULT_DAEMON_URL) + '/api/evy/chat'
  const body = JSON.stringify({
    session_id: opts.sessionId,
    message: opts.message
  })

  let resp: Response
  try {
    resp = await fetch(url, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        accept: 'text/event-stream'
      },
      body,
      signal: opts.signal
    })
  } catch (err) {
    yield {
      kind: 'error',
      error_kind: 'transport',
      message: `connect failed: ${String(err)}`
    }
    return
  }

  if (!resp.ok) {
    // Non-2xx during the streaming branch is rare (the handler upgrades
    // to 200 once headers flush) but possible for early failures like
    // 503 unavailable, 400 bad request.
    const text = await resp.text().catch(() => '')
    yield {
      kind: 'error',
      error_kind: `http_${resp.status}`,
      message: text || `HTTP ${resp.status}`
    }
    return
  }

  const body_stream = resp.body
  if (!body_stream) {
    yield {
      kind: 'error',
      error_kind: 'transport',
      message: 'response had no body'
    }
    return
  }

  const reader = body_stream.getReader()
  const decoder = new TextDecoder('utf-8')
  const parser = new SseChunkParser()

  try {
    while (true) {
      const { value, done } = await reader.read()
      if (done) {
        for (const ev of parser.flush()) {
          yield ev
        }
        return
      }
      const chunk = decoder.decode(value, { stream: true })
      for (const ev of parser.push(chunk)) {
        yield ev
        if (ev.kind === 'done' || ev.kind === 'error') {
          // Daemon should already close after this; bail out so we don't
          // hold the reader open longer than necessary.
          return
        }
      }
    }
  } catch (err) {
    if (opts.signal.aborted) {
      // Client-initiated abort — silent.
      return
    }
    yield {
      kind: 'error',
      error_kind: 'transport',
      message: `stream error: ${String(err)}`
    }
  } finally {
    try {
      reader.releaseLock()
    } catch {
      // ignore — already released or stream errored
    }
  }
}

async function getJson<T>(url: string, signal?: AbortSignal): Promise<T> {
  let resp: Response
  try {
    resp = await fetch(url, {
      headers: { accept: 'application/json' },
      signal
    })
  } catch (err) {
    throw new GatewayError(`connect failed: ${String(err)}`, err)
  }
  if (!resp.ok) {
    throw new GatewayError(`HTTP ${resp.status} ${resp.statusText}`)
  }
  return (await resp.json()) as T
}

export async function getHealth(opts?: {
  url?: string
  signal?: AbortSignal
}): Promise<HealthResponse> {
  return getJson<HealthResponse>(
    (opts?.url ?? DEFAULT_DAEMON_URL) + '/health',
    opts?.signal
  )
}

export async function listSessions(opts?: {
  url?: string
  signal?: AbortSignal
}): Promise<SessionsListResponse> {
  return getJson<SessionsListResponse>(
    (opts?.url ?? DEFAULT_DAEMON_URL) + '/api/evy/sessions',
    opts?.signal
  )
}

export async function listSkills(opts?: {
  url?: string
  signal?: AbortSignal
}): Promise<SkillsListResponse> {
  return getJson<SkillsListResponse>(
    (opts?.url ?? DEFAULT_DAEMON_URL) + '/api/evy/skills',
    opts?.signal
  )
}

export async function deleteSession(opts: {
  url?: string
  sessionId: string
  signal?: AbortSignal
}): Promise<void> {
  const url =
    (opts.url ?? DEFAULT_DAEMON_URL) + '/api/evy/sessions/' + opts.sessionId
  let resp: Response
  try {
    resp = await fetch(url, { method: 'DELETE', signal: opts.signal })
  } catch (err) {
    throw new GatewayError(`connect failed: ${String(err)}`, err)
  }
  if (!resp.ok) {
    throw new GatewayError(`HTTP ${resp.status} ${resp.statusText}`)
  }
}
