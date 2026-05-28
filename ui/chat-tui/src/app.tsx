// Root App component — wires the chat surface, slash dispatcher, gateway
// streaming, and daemon autostart together.
//
// Layout (matching Hermes appLayout.tsx):
//   ┌────────────────────────────────────────┐
//   │  Banner (only at top, first paint)    │
//   │  ── transcript (flex-grow) ──         │
//   │      MessageRow ×N                     │
//   │      StreamingRow (if isStreaming)     │
//   │  ── composer (flex-shrink-0) ──       │
//   │      ❯ <input>                         │
//   │      StatusRule                        │
//   └────────────────────────────────────────┘

import { useStore } from '@nanostores/react'
import { Box, Text, useApp, useStdout } from 'ink'
import React, { useCallback, useEffect, useRef, useState } from 'react'

import { Banner } from './chat/banner.js'
import { MessageRow, StreamingRow } from './chat/messages.js'
import { StatusRule } from './chat/statusRule.js'
import { ensureDaemonRunning } from './daemon/autostart.js'
import { streamChat } from './gateway/client.js'
import { Composer } from './input/composer.js'
import { dispatchSlash } from './slash/commands.js'
import { parseSlashCommand } from './slash/parser.js'
import { readBackend } from './slash/config.js'
import {
  $transcript,
  appendSystem,
  appendToken,
  appendUser,
  beginAssistantTurn,
  completeAssistantTurn,
  failAssistantTurn,
  recordSkillLoaded
} from './state/transcript.js'
import { $ui, patchUi } from './state/ui.js'

export function App(): React.ReactElement {
  const { exit } = useApp()
  const ui = useStore($ui)
  const transcript = useStore($transcript)
  const { stdout } = useStdout()

  const [history, setHistory] = useState<string[]>([])
  const [caretVisible, setCaretVisible] = useState(true)
  const abortRef = useRef<AbortController | null>(null)
  const bootRef = useRef(false)

  // Boot once: daemon autostart + read backend from config.
  useEffect(() => {
    if (bootRef.current) {
      return
    }
    bootRef.current = true
    void (async () => {
      patchUi({ daemonStatus: 'starting' })
      const state = await ensureDaemonRunning({ timeoutMs: 5000 })
      if (state.status === 'running') {
        patchUi({
          daemonStatus: 'running',
          daemonVersion: state.version ?? null,
          daemonError: null
        })
        appendSystem(
          `connected · daemon v${state.version ?? '?'} · type /help for commands`
        )
      } else {
        patchUi({
          daemonStatus: state.status === 'absent' ? 'absent' : 'failed',
          daemonError: state.detail ?? null
        })
        appendSystem(
          `[daemon] ${state.status}${state.detail ? ' · ' + state.detail : ''}`
        )
      }
      const backend = await readBackend().catch(() => null)
      if (backend) {
        patchUi({ backend })
      }
    })()
  }, [])

  // Blink the streaming caret.
  useEffect(() => {
    if (!transcript.isStreaming) {
      setCaretVisible(true)
      return
    }
    const id = setInterval(() => setCaretVisible((v) => !v), 420)
    return () => clearInterval(id)
  }, [transcript.isStreaming])

  // Submission pipeline — slash dispatch first, then chat stream.
  const handleSubmit = useCallback(
    async (text: string) => {
      // Push history.
      setHistory((h) => (h[h.length - 1] === text ? h : [...h, text]))

      const slash = parseSlashCommand(text)
      if (slash) {
        const outcome = await dispatchSlash(slash)
        if (outcome.kind === 'exit') {
          exit()
        } else if (outcome.kind === 'unknown') {
          appendSystem(`unknown command: /${slash.name} (try /help)`)
        }
        return
      }

      if (ui.daemonStatus !== 'running') {
        appendSystem(
          '[daemon] not running — type /status to recheck or restart the daemon.'
        )
        return
      }

      appendUser(text)
      beginAssistantTurn()

      const ctrl = new AbortController()
      abortRef.current = ctrl
      let lastSessionId: string | null = transcript.sessionId

      try {
        for await (const ev of streamChat({
          sessionId: transcript.sessionId,
          message: text,
          signal: ctrl.signal
        })) {
          if (ev.kind === 'token') {
            appendToken(ev.content)
          } else if (ev.kind === 'skill_loaded') {
            recordSkillLoaded(ev.name)
          } else if (ev.kind === 'done') {
            lastSessionId = ev.session_id
            completeAssistantTurn(ev.session_id)
            return
          } else if (ev.kind === 'error') {
            failAssistantTurn(`${ev.error_kind}: ${ev.message}`)
            return
          }
        }
        // Stream ended without `done` or `error` — flush whatever we have.
        if ($transcript.get().isStreaming) {
          if (lastSessionId) {
            completeAssistantTurn(lastSessionId)
          } else {
            failAssistantTurn('stream ended before session_id arrived')
          }
        }
      } catch (err) {
        if (!ctrl.signal.aborted) {
          failAssistantTurn(String(err))
        }
      } finally {
        abortRef.current = null
      }
    },
    [exit, transcript.sessionId, ui.daemonStatus]
  )

  const handleInterrupt = useCallback(() => {
    if (abortRef.current) {
      abortRef.current.abort()
      abortRef.current = null
      failAssistantTurn('interrupted by operator')
    }
  }, [])

  const handleExit = useCallback(() => {
    if (abortRef.current) {
      abortRef.current.abort()
    }
    appendSystem(ui.theme.brand.goodbye)
    setTimeout(() => exit(), 50)
  }, [exit, ui.theme.brand.goodbye])

  // Render the chat surface.
  const t = ui.theme

  // First user message in this render — used to decide whether to print
  // the `───` separator above subsequent user msgs.
  let firstUserSeen = false

  // For very wide transcripts, only render the last N rows so Ink doesn't
  // have to repaint the whole history every frame. 200 rows is generous;
  // virtualisation lands in Tier 3.
  const VISIBLE_ROWS = 200
  const visibleMessages = transcript.messages.slice(-VISIBLE_ROWS)

  // Optional terminal width info — useful once we wire markdown rendering.
  void stdout?.columns

  return (
    <Box flexDirection="column" paddingX={1}>
      <Banner
        theme={t}
        daemonStatus={ui.daemonStatus}
        daemonVersion={ui.daemonVersion}
        daemonDetail={ui.daemonError}
      />

      {/* Transcript — last N messages */}
      <Box flexDirection="column">
        {visibleMessages.map((msg) => {
          const isUser = msg.role === 'user'
          const isFirstUser = isUser && !firstUserSeen
          if (isUser) {
            firstUserSeen = true
          }
          return (
            <MessageRow
              key={msg.id}
              theme={t}
              msg={msg}
              isFirstUser={isFirstUser}
            />
          )
        })}
        {transcript.isStreaming ? (
          <StreamingRow
            theme={t}
            text={transcript.streaming}
            liveSkills={transcript.liveSkills}
            caretVisible={caretVisible}
          />
        ) : null}
      </Box>

      {/* Composer */}
      <Box flexDirection="column" marginTop={1}>
        <Box>
          <Text color={t.color.prompt} bold>
            ❯{' '}
          </Text>
          <Composer
            theme={t}
            disabled={transcript.isStreaming}
            onSubmit={(text) => {
              void handleSubmit(text)
            }}
            onInterrupt={handleInterrupt}
            onExit={handleExit}
            placeholder={
              transcript.isStreaming
                ? 'streaming · Ctrl+C to interrupt'
                : 'message Evy · /help for commands'
            }
            history={history}
          />
        </Box>
        <Box marginTop={1}>
          <StatusRule
            theme={t}
            daemonStatus={ui.daemonStatus}
            daemonVersion={ui.daemonVersion}
            backend={ui.backend}
            sessionId={transcript.sessionId}
            isStreaming={transcript.isStreaming}
            messageCount={transcript.messages.length}
          />
        </Box>
      </Box>
    </Box>
  )
}
