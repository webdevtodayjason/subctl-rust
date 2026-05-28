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
  const hasMeta = (msg.skillsLoaded?.length ?? 0) > 0
  return (
    <Box flexDirection="column" marginTop={1}>
      {hasMeta && msg.skillsLoaded && msg.skillsLoaded.length > 0 ? (
        <Box flexDirection="column">
          <Box>
            <Text color={t.color.border}>{t.brand.tool} </Text>
            <Text color={t.color.muted} dimColor>
              {`├─ skills loaded · ${msg.skillsLoaded.length}`}
            </Text>
          </Box>
          {/* Show up to 3 skill names so the operator sees what landed. */}
          {msg.skillsLoaded.slice(0, 3).map((s) => (
            <Box key={s}>
              <Text color={t.color.border}>{t.brand.tool}   </Text>
              <Text color={t.color.muted} dimColor>
                ·{' '}
              </Text>
              <Text color={t.color.muted}>{s}</Text>
            </Box>
          ))}
          {msg.skillsLoaded.length > 3 ? (
            <Box>
              <Text color={t.color.border}>{t.brand.tool}   </Text>
              <Text color={t.color.muted} dimColor>
                · …+{msg.skillsLoaded.length - 3} more
              </Text>
            </Box>
          ) : null}
          <Box>
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
}

export function StreamingRow(
  props: StreamingRowProps
): React.ReactElement {
  const { theme: t, text, liveSkills, caretVisible } = props
  return (
    <Box flexDirection="column" marginTop={1}>
      {liveSkills.length > 0 ? (
        <Box>
          <Text color={t.color.border}>{t.brand.tool} </Text>
          <Text color={t.color.muted} dimColor>
            {`├─ loaded ${liveSkills.length} skill${liveSkills.length === 1 ? '' : 's'}`}
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
