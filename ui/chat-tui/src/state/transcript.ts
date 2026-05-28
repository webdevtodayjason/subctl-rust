// Transcript store — append-only list of committed turns + the in-flight
// assistant text. Inspired by Hermes's $turnState but flattened for the
// v0.1 chat surface (no tool calls, no reasoning split, no overlays).
//
// `streaming` holds the live partial reply; on `done` we move it to a
// committed assistant Msg and clear the buffer. The prefix-cache invariant
// from spec §4 is preserved at the render boundary, not here — the store
// just appends tokens to `streaming`.

import { atom } from 'nanostores'

export interface Msg {
  /** Stable id for keys in React lists. */
  id: number
  role: 'user' | 'assistant' | 'system'
  /** Body text. May be empty (in-flight assistant pre-token, system spinners). */
  text: string
  /** Skills the daemon announced this turn (assistant only, may be empty). */
  skillsLoaded?: string[]
}

export interface TranscriptState {
  messages: Msg[]
  /** Live assistant reply being streamed (empty when idle). */
  streaming: string
  /** True from turn start until `done`/`error`. */
  isStreaming: boolean
  /** Session id once the daemon assigns one. */
  sessionId: string | null
  /** Last error surfaced from the gateway (sticky until next submit). */
  lastError: string | null
  /** Skills announced during the current/most recent assistant turn. */
  liveSkills: string[]
}

const initial: TranscriptState = {
  messages: [],
  streaming: '',
  isStreaming: false,
  sessionId: null,
  lastError: null,
  liveSkills: []
}

export const $transcript = atom<TranscriptState>(initial)

let nextId = 1
function newId(): number {
  return nextId++
}

export function appendUser(text: string): void {
  const state = $transcript.get()
  $transcript.set({
    ...state,
    messages: [...state.messages, { id: newId(), role: 'user', text }],
    lastError: null
  })
}

export function appendSystem(text: string): void {
  const state = $transcript.get()
  $transcript.set({
    ...state,
    messages: [...state.messages, { id: newId(), role: 'system', text }]
  })
}

export function beginAssistantTurn(): void {
  $transcript.set({
    ...$transcript.get(),
    streaming: '',
    isStreaming: true,
    liveSkills: []
  })
}

// Token-flush batching — Hermes batches at 16ms (STREAM_BATCH_MS, spec §4)
// because per-token re-renders make Ink's screen-diff cost dominate.
let _tokenBuf = ''
let _tokenTimer: NodeJS.Timeout | null = null
const STREAM_BATCH_MS = 16

function flushTokens(): void {
  if (_tokenBuf === '') {
    _tokenTimer = null
    return
  }
  const state = $transcript.get()
  $transcript.set({ ...state, streaming: state.streaming + _tokenBuf })
  _tokenBuf = ''
  _tokenTimer = null
}

export function appendToken(chunk: string): void {
  _tokenBuf += chunk
  if (_tokenTimer === null) {
    _tokenTimer = setTimeout(flushTokens, STREAM_BATCH_MS)
  }
}

/** Flush pending tokens synchronously — call before committing/failing a turn. */
function drainTokenBuffer(): void {
  if (_tokenTimer !== null) {
    clearTimeout(_tokenTimer)
    _tokenTimer = null
  }
  if (_tokenBuf !== '') {
    const state = $transcript.get()
    $transcript.set({ ...state, streaming: state.streaming + _tokenBuf })
    _tokenBuf = ''
  }
}

// The daemon emits a skill_loaded frame for every registered skill at the
// start of a turn (~100 in production), so per-event $transcript.set fires
// ~100 React re-renders before the first token arrives.  Mirror the
// 16ms token-batching pattern: collect names into a Set, flush in one set.
let _skillBuf: Set<string> = new Set()
let _skillTimer: NodeJS.Timeout | null = null

function flushSkills(): void {
  if (_skillBuf.size === 0) {
    _skillTimer = null
    return
  }
  const state = $transcript.get()
  const merged = state.liveSkills.slice()
  for (const name of _skillBuf) {
    if (!merged.includes(name)) {
      merged.push(name)
    }
  }
  $transcript.set({ ...state, liveSkills: merged })
  _skillBuf = new Set()
  _skillTimer = null
}

export function recordSkillLoaded(name: string): void {
  _skillBuf.add(name)
  if (_skillTimer === null) {
    _skillTimer = setTimeout(flushSkills, STREAM_BATCH_MS)
  }
}

/** Force-drain the skill buffer alongside the token buffer at turn end. */
function drainSkillBuffer(): void {
  if (_skillTimer !== null) {
    clearTimeout(_skillTimer)
    _skillTimer = null
  }
  if (_skillBuf.size > 0) {
    flushSkills()
  }
}

export function completeAssistantTurn(sessionId: string): void {
  drainTokenBuffer()
  drainSkillBuffer()
  const state = $transcript.get()
  const text = state.streaming
  const skillsLoaded = state.liveSkills
  // Even with no token frames we still commit an empty message — the
  // operator should see the turn boundary even if the daemon returned
  // nothing renderable.
  const newMsgs: Msg[] = [
    ...state.messages,
    { id: newId(), role: 'assistant', text, skillsLoaded }
  ]
  $transcript.set({
    ...state,
    messages: newMsgs,
    streaming: '',
    isStreaming: false,
    sessionId,
    liveSkills: []
  })
}

export function failAssistantTurn(message: string): void {
  drainTokenBuffer()
  drainSkillBuffer()
  const state = $transcript.get()
  // Roll back the streaming buffer; surface the error as a system msg so
  // it remains visible across turns.
  $transcript.set({
    ...state,
    messages: [
      ...state.messages,
      { id: newId(), role: 'system', text: `[error] ${message}` }
    ],
    streaming: '',
    isStreaming: false,
    lastError: message,
    liveSkills: []
  })
}

/** `/clear` — drop transcript + session. Used by `/new-session` too. */
export function resetTranscript(): void {
  $transcript.set({
    messages: [],
    streaming: '',
    isStreaming: false,
    sessionId: null,
    lastError: null,
    liveSkills: []
  })
}
