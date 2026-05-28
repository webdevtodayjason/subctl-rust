// Hermes-port: per-turn `───` between user turns + `└─ Response` between
// the assistant's meta block and reply body.
//
// Citations from /Users/sem/code/subctl-rust-hermes-uitui/docs/hermes-uitui-spec.md:
//   §12 pattern #1 — per-turn separator (appLayout.tsx:107-111)
//   §12 pattern #2 — response separator (messageLine.tsx:200-209)

/** Three-cell em-dash run, color `border`. Dim. */
export const TURN_SEPARATOR = '───'

/** `└─ Response` label, color `border` on the lead-in, `muted` dim on the word. */
export const RESPONSE_PREFIX = '└─ '
export const RESPONSE_LABEL = 'Response'
