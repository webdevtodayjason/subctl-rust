// Message renderer — one row per Msg, with the Hermes role-glyph + body
// convention:
//
//   user        ❯ (bold, label color)
//   assistant   ┊ (border color) + `└─ Response` separator if there's meta
//   system      · (muted, dim)
//
// Per-turn `───` separator between user turns (spec §12 pattern #1).
// `└─ Response` separator (§12 pattern #2) — we only render it when the
// assistant message has *skill loads* attached, since we don't (yet) have
// tool calls or reasoning to gate on.

import { Box, Text } from 'ink'
import React from 'react'

import { TURN_SEPARATOR, RESPONSE_PREFIX, RESPONSE_LABEL } from './separators.js'
import type { Msg } from '../state/transcript.js'
import type { Theme } from '../theme.js'

interface MessageRowProps {
  theme: Theme
  msg: Msg
  /** True for the first user msg per render (skips the separator). */
  isFirstUser: boolean
}

export function MessageRow(props: MessageRowProps): React.ReactElement {
  const { theme: t, msg, isFirstUser } = props

  if (msg.role === 'user') {
    return (
      <Box flexDirection="column">
        {!isFirstUser ? (
          <Box marginTop={1}>
            <Text color={t.color.border} dimColor>
              {TURN_SEPARATOR}
            </Text>
          </Box>
        ) : null}
        <Box>
          <Text color={t.color.label} bold>
            ❯{' '}
          </Text>
          <Text color={t.color.label}>{msg.text}</Text>
        </Box>
      </Box>
    )
  }

  if (msg.role === 'system') {
    return (
      <Box>
        <Text color={t.color.muted} dimColor>
          ·{' '}
        </Text>
        <Text color={t.color.muted} dimColor>
          {msg.text}
        </Text>
      </Box>
    )
  }

  // assistant
  //
  // Meta block + `└─ Response` separator only renders when the daemon
  // loaded a handful of skills this turn — the registry has 100+ entries
  // and the daemon currently emits a `skill_loaded` for each, which would
  // turn the meta block into a wall of noise.  Operator sees the full
  // registry via /skills.  Threshold is intentionally conservative; the
  // §12 pattern was designed for sparse "the model autoloaded X" signal.
  const skills = msg.skillsLoaded ?? []
  const showMeta = skills.length > 0 && skills.length <= 5
  return (
    <Box flexDirection="column" marginTop={1}>
      {showMeta ? (
        <Box flexDirection="column">
          {skills.map((s) => (
            <Box key={s}>
              <Text color={t.color.border}>{t.brand.tool} </Text>
              <Text color={t.color.muted} dimColor>
                ├─ skill ·{' '}
              </Text>
              <Text color={t.color.muted}>{s}</Text>
            </Box>
          ))}
          <Box>
            <Text color={t.color.border}>{t.brand.tool} </Text>
            <Text color={t.color.border}>{RESPONSE_PREFIX}</Text>
            <Text color={t.color.muted} dimColor>
              {RESPONSE_LABEL}
            </Text>
          </Box>
        </Box>
      ) : null}
      <Box>
        <Text color={t.color.border}>{t.brand.tool} </Text>
        <Text color={t.color.text}>{msg.text || ' '}</Text>
      </Box>
    </Box>
  )
}

/**
 * Live in-flight assistant row — rendered when `isStreaming` flips on,
 * before the message commits. Same glyph as a committed assistant msg
 * but with a blinking caret.
 */
interface StreamingRowProps {
  theme: Theme
  text: string
  liveSkills: readonly string[]
  caretVisible: boolean
  spinnerFrame: string
}

export function StreamingRow(
  props: StreamingRowProps
): React.ReactElement {
  const { theme: t, text, liveSkills, caretVisible, spinnerFrame } = props

  // The daemon emits a `skill_loaded` event for every registered skill
  // (100+ entries) before the first token frame.  We render that as a
  // single dim "scanning skills…" line — never a count, never names —
  // so the operator sees activity without being shouted at.  If by some
  // chance only a handful are loaded, we'll surface them after the turn
  // commits (see MessageRow).
  return (
    <Box flexDirection="column" marginTop={1}>
      {liveSkills.length > 0 && text === '' ? (
        <Box>
          <Text color={t.color.border}>{t.brand.tool} </Text>
          <Text color={t.color.accent}>{spinnerFrame}</Text>
          <Text color={t.color.muted} dimColor>
            {' scanning skills…'}
          </Text>
        </Box>
      ) : null}
      <Box>
        <Text color={t.color.border}>{t.brand.tool} </Text>
        <Text color={t.color.text}>{text}</Text>
        <Text color={t.color.accent}>{caretVisible ? '▍' : ' '}</Text>
      </Box>
    </Box>
  )
}
