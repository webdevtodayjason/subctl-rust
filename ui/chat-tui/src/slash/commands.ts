// Slash command dispatcher. Each command is a tiny async function the
// chat App invokes with a context bag. Commands surface user-facing output
// via `appendSystem(...)` and may signal exit/clear via the returned
// SlashOutcome.
//
// Tier 1 set: /help /quit /q /exit /clear /new-session /backend
//             /sessions /skills /status

import type { SlashCommand } from './parser.js'
import {
  $transcript,
  appendSystem,
  resetTranscript
} from '../state/transcript.js'
import { $ui, patchUi } from '../state/ui.js'
import {
  deleteSession,
  getHealth,
  GatewayError,
  listSessions,
  listSkills
} from '../gateway/client.js'
import { isBackendChoice, readBackend, writeBackend } from './config.js'
import { CONFIG_TOML_PATH, DEFAULT_DAEMON_URL } from '../constants.js'

export type SlashOutcome =
  | { kind: 'handled' }
  | { kind: 'exit' }
  | { kind: 'unknown' }

export async function dispatchSlash(
  cmd: SlashCommand
): Promise<SlashOutcome> {
  switch (cmd.name) {
    case 'help':
      return cmdHelp()
    case 'quit':
    case 'q':
    case 'exit':
      return cmdQuit()
    case 'clear':
    case 'new-session':
    case 'new':
      return cmdClear()
    case 'backend':
      return cmdBackend(cmd.argv)
    case 'sessions':
      return cmdSessions(cmd.argv)
    case 'skills':
      return cmdSkills()
    case 'status':
      return cmdStatus()
    default:
      return { kind: 'unknown' }
  }
}

function cmdHelp(): SlashOutcome {
  const lines = [
    'Commands:',
    '  /help                  show this list',
    '  /quit · /q · /exit     leave the TUI',
    '  /clear · /new-session  drop the current session and start fresh',
    '  /sessions [delete ID]  list past sessions; optionally delete one',
    '  /skills                list skills the daemon has registered',
    '  /status                show daemon /health (version, backend)',
    '  /backend lm-studio | codex | anthropic | stub',
    '                         switch thinking-partner backend (config.toml)',
    '',
    'Editing:',
    '  Enter                  submit',
    '  Alt+Enter | \\+Enter    insert newline',
    '  Ctrl+C                 interrupt stream → clear draft → exit',
    '  Ctrl+W                 delete word left',
    '  Ctrl+U / Ctrl+K        kill to start / end of line',
    '  Ctrl+A / Ctrl+E        jump to start / end of line',
    '  ↑ / ↓                  cycle prompt history',
    '  Esc                    exit'
  ]
  appendSystem(lines.join('\n'))
  return { kind: 'handled' }
}

function cmdQuit(): SlashOutcome {
  return { kind: 'exit' }
}

function cmdClear(): SlashOutcome {
  resetTranscript()
  appendSystem('session cleared — next message opens a new session.')
  return { kind: 'handled' }
}

async function cmdBackend(argv: readonly string[]): Promise<SlashOutcome> {
  if (argv.length === 0) {
    const current = (await readBackend().catch(() => null)) ?? '(unset — daemon default)'
    appendSystem(
      `current backend: ${current}\n` +
        `usage: /backend lm-studio | codex | anthropic | stub`
    )
    return { kind: 'handled' }
  }
  const choice = argv[0] ?? ''
  if (!isBackendChoice(choice)) {
    appendSystem(
      `[/backend] unknown backend: "${choice}". ` +
        `Choose one of: lm-studio, codex, anthropic, stub.`
    )
    return { kind: 'handled' }
  }
  try {
    await writeBackend(choice)
    patchUi({ backend: choice })
    appendSystem(
      `backend set to "${choice}" in ${CONFIG_TOML_PATH}. ` +
        `Restart the daemon for the change to take effect ` +
        `(launchctl unload && launchctl load …).`
    )
  } catch (err) {
    appendSystem(`[/backend] write failed: ${String(err)}`)
  }
  return { kind: 'handled' }
}

async function cmdSessions(argv: readonly string[]): Promise<SlashOutcome> {
  if (argv[0] === 'delete' && argv[1]) {
    const id = argv[1]
    try {
      await deleteSession({ sessionId: id })
      appendSystem(`deleted session ${id}.`)
    } catch (err) {
      appendSystem(`[/sessions] delete failed: ${gatewayMsg(err)}`)
    }
    return { kind: 'handled' }
  }
  try {
    const resp = await listSessions()
    if (resp.sessions.length === 0) {
      appendSystem('no sessions on the daemon yet.')
      return { kind: 'handled' }
    }
    const lines = ['sessions (newest first):']
    for (const s of resp.sessions) {
      const preview = s.preview || '(no topic)'
      lines.push(
        `  ${s.id.slice(0, 8)} · ${s.status} · ${s.message_count} msgs · ${preview}`
      )
    }
    lines.push('use /sessions delete <id> to drop one.')
    appendSystem(lines.join('\n'))
  } catch (err) {
    appendSystem(`[/sessions] list failed: ${gatewayMsg(err)}`)
  }
  return { kind: 'handled' }
}

async function cmdSkills(): Promise<SlashOutcome> {
  try {
    const resp = await listSkills()
    if (resp.skills.length === 0) {
      appendSystem('no skills registered on the daemon.')
      return { kind: 'handled' }
    }
    const lines = [`skills (${resp.skills.length} registered):`]
    // Show 20 to keep the dump readable; full list is at /api/evy/skills.
    for (const s of resp.skills.slice(0, 20)) {
      const desc = s.description || '(no description)'
      lines.push(`  ${s.name} — ${desc}`)
    }
    if (resp.skills.length > 20) {
      lines.push(`  …+${resp.skills.length - 20} more`)
    }
    appendSystem(lines.join('\n'))
  } catch (err) {
    appendSystem(`[/skills] list failed: ${gatewayMsg(err)}`)
  }
  return { kind: 'handled' }
}

async function cmdStatus(): Promise<SlashOutcome> {
  try {
    const h = await getHealth()
    patchUi({
      daemonStatus: 'running',
      daemonVersion: h.version,
      daemonError: null
    })
    const ui = $ui.get()
    const sessionId = $transcript.get().sessionId
    const lines = [
      `daemon: running · v${h.version}`,
      `endpoint: ${DEFAULT_DAEMON_URL}`,
      `backend: ${ui.backend ?? '(daemon default)'}`,
      `session: ${sessionId ?? '(none yet — next message opens one)'}`
    ]
    appendSystem(lines.join('\n'))
  } catch (err) {
    patchUi({ daemonStatus: 'failed', daemonError: String(err) })
    appendSystem(`[/status] daemon health check failed: ${gatewayMsg(err)}`)
  }
  return { kind: 'handled' }
}

function gatewayMsg(err: unknown): string {
  if (err instanceof GatewayError) {
    return err.message
  }
  return String(err)
}
