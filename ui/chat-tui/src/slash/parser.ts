// Slash-command parser.
//
// A submission qualifies as a slash command iff it starts with '/' and a
// letter (so '/' alone, or '/123', stays as user input — useful when the
// operator is literally talking about a path).

export interface SlashCommand {
  /** Lowercase command name without the leading '/'. */
  name: string
  /** Argument string (everything after the first whitespace), trimmed. */
  args: string
  /** Argv-style breakdown for callers that want tokens not raw text. */
  argv: string[]
}

export function looksLikeSlashCommand(input: string): boolean {
  // Trim leading whitespace so paste / accidental space doesn't kick the
  // submission back to the LLM as prose.
  return /^\/[a-zA-Z]/.test(input.trimStart())
}

/**
 * Parse a slash input. Returns `null` if the input isn't a slash command.
 *
 * Tokenisation is naive whitespace-split — quotes are not honoured. The
 * v0.1 slash surface (/quit, /help, /clear, /backend X, /sessions, /skills,
 * /status, /new-session) doesn't need richer parsing; if a future command
 * does we revisit.
 */
export function parseSlashCommand(input: string): SlashCommand | null {
  if (!looksLikeSlashCommand(input)) {
    return null
  }
  const trimmed = input.trim()
  const wsIdx = trimmed.search(/\s/)
  const cmd = (wsIdx === -1 ? trimmed.slice(1) : trimmed.slice(1, wsIdx)).toLowerCase()
  const rest = wsIdx === -1 ? '' : trimmed.slice(wsIdx + 1).trim()
  const argv = rest === '' ? [] : rest.split(/\s+/)
  return { name: cmd, args: rest, argv }
}
