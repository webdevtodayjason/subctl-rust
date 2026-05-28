// Wire types — mirror the Rust shapes in crates/evy-comms exactly.
//
// Source of truth:
// - ChatRequest / ChatResponse  → crates/evy-comms/src/chat.rs:81, 92
// - ChatStreamEvent             → crates/evy-comms/src/chat.rs:214
// - SessionsListResponse        → crates/evy-comms/src/sessions_http.rs:70
// - SkillsListResponse          → crates/evy-comms/src/skills_http.rs:54
//
// We don't import them; we restate them as TS so the TUI ships
// independent of the Rust crate. Any drift between this file and the Rust
// side is a bug — keep both in sync when the Rust shapes change.

export interface ChatRequest {
  /** `null`/omitted opens a new session; `Some(uuid)` appends. */
  session_id?: string | null
  /** Operator text. Daemon trims; must be non-empty after trim. */
  message: string
}

export interface ChatResponse {
  session_id: string
  response: string
  skills_loaded: string[]
}

/**
 * Discriminated union over the four SSE event shapes the chat handler
 * emits. Matches `ChatStreamEvent` in `chat.rs` exactly, including the
 * `snake_case` rename on the `kind` tag.
 */
export type ChatStreamEvent =
  | { kind: 'token'; content: string }
  | { kind: 'skill_loaded'; name: string }
  | { kind: 'done'; session_id: string }
  | { kind: 'error'; error_kind: string; message: string }

export interface SessionSummary {
  id: string
  started_at: string
  last_message_at: string
  message_count: number
  preview: string
  status: 'active' | 'concluded' | 'timed_out' | string
}

export interface SessionsListResponse {
  sessions: SessionSummary[]
}

export interface SkillSummary {
  name: string
  description: string
  triggers: string[]
  priority: number
}

export interface SkillsListResponse {
  skills: SkillSummary[]
}

export interface HealthResponse {
  ok: boolean
  version: string
}
