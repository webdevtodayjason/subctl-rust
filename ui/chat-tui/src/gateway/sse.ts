// SSE line buffer — turns a stream of partial chunks into discrete events.
//
// EventSource wire format (per HTML living standard):
//   each frame is one-or-more lines, terminated by a blank line
//   lines starting with `data:` carry the payload (we ignore `event:`,
//   `id:`, `retry:` because the v4 chat handler only emits `data:`)
//
// Our `data` is always one JSON object. We concatenate consecutive
// `data:` lines into a single string (per spec, separated by `\n`) and
// flush on blank-line boundary.
//
// Network chunks arrive arbitrarily — could be 1 char or 64KB. We hold a
// rolling buffer of the unfinished tail and pop completed events off the
// front each time push() is called.

import type { ChatStreamEvent } from './types.js'

/**
 * Stateful chunk-to-event splitter. One instance per HTTP response.
 *
 * Usage:
 *   const parser = new SseChunkParser()
 *   for (const chunk of stream) {
 *     for (const ev of parser.push(chunk)) { ... }
 *   }
 *   for (const ev of parser.flush()) { ... }  // optional, drains any
 *                                                trailing event without
 *                                                a closing blank line
 */
export class SseChunkParser {
  private buf = ''
  private dataLines: string[] = []

  /**
   * Feed a chunk of decoded UTF-8 from the response body. Returns the
   * fully-parsed events the chunk completed (zero or more).
   */
  push(chunk: string): ChatStreamEvent[] {
    this.buf += chunk

    const events: ChatStreamEvent[] = []

    // CRLF and CR both normalised to LF, then we split on LF.
    // (EventSource spec allows all three terminators.)
    this.buf = this.buf.replace(/\r\n/g, '\n').replace(/\r/g, '\n')

    let nl = this.buf.indexOf('\n')
    while (nl !== -1) {
      const line = this.buf.slice(0, nl)
      this.buf = this.buf.slice(nl + 1)

      if (line === '') {
        // Blank line = dispatch.
        const ev = this.dispatch()
        if (ev !== null) {
          events.push(ev)
        }
      } else if (line.startsWith('data:')) {
        // Spec: strip a single leading space after the colon if present.
        const rest = line.slice(5)
        this.dataLines.push(rest.startsWith(' ') ? rest.slice(1) : rest)
      }
      // Comments (`:`-prefixed) and other fields ignored.

      nl = this.buf.indexOf('\n')
    }

    return events
  }

  /**
   * Drain any pending event without a closing blank line. The v4 daemon
   * always terminates frames properly, so this is defensive — returns
   * empty in the happy path.
   */
  flush(): ChatStreamEvent[] {
    const events: ChatStreamEvent[] = []
    if (this.buf.length > 0 && this.buf.startsWith('data:')) {
      const rest = this.buf.slice(5)
      this.dataLines.push(rest.startsWith(' ') ? rest.slice(1) : rest)
      this.buf = ''
    }
    const ev = this.dispatch()
    if (ev !== null) {
      events.push(ev)
    }
    return events
  }

  private dispatch(): ChatStreamEvent | null {
    if (this.dataLines.length === 0) {
      return null
    }
    const payload = this.dataLines.join('\n')
    this.dataLines = []

    try {
      const parsed = JSON.parse(payload) as ChatStreamEvent
      if (!parsed || typeof parsed !== 'object' || !('kind' in parsed)) {
        return null
      }
      return parsed
    } catch (err) {
      // Malformed frame — surface as an error event so the caller doesn't
      // hang waiting for `done`/`error`.
      return {
        kind: 'error',
        error_kind: 'parse_error',
        message: `failed to parse SSE frame: ${String(err)}`
      }
    }
  }
}
