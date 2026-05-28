// Footer status rule — single row, color-coded sections separated by `│`.
// Tier 2 polish; ships a competent baseline now so the chrome reads
// finished.

import { Box, Text } from 'ink'
import React from 'react'

import type { Theme } from '../theme.js'

interface StatusRuleProps {
  theme: Theme
  daemonStatus: string
  daemonVersion: string | null
  backend: string | null
  sessionId: string | null
  isStreaming: boolean
  messageCount: number
}

export function StatusRule(props: StatusRuleProps): React.ReactElement {
  const {
    theme: t,
    daemonStatus,
    daemonVersion,
    backend,
    sessionId,
    isStreaming,
    messageCount
  } = props

  const segments: React.ReactNode[] = []
  segments.push(
    <Text key="d" color={daemonColor(t, daemonStatus)}>
      {`◆ ${daemonStatus}${daemonVersion ? ' v' + daemonVersion : ''}`}
    </Text>
  )
  segments.push(
    <Text key="b" color={t.color.muted}>
      {backend ? `backend: ${backend}` : 'backend: default'}
    </Text>
  )
  segments.push(
    <Text key="s" color={t.color.muted}>
      {`session: ${sessionId ? sessionId.slice(0, 8) : '—'}`}
    </Text>
  )
  segments.push(
    <Text key="m" color={t.color.muted}>
      {`msgs: ${messageCount}`}
    </Text>
  )
  if (isStreaming) {
    segments.push(
      <Text key="t" color={t.color.accent}>
        streaming…
      </Text>
    )
  }

  return (
    <Box>
      {segments.map((seg, i) => (
        <React.Fragment key={i}>
          {i > 0 ? (
            <Text color={t.color.border} dimColor>
              {' │ '}
            </Text>
          ) : null}
          {seg}
        </React.Fragment>
      ))}
    </Box>
  )
}

function daemonColor(t: Theme, status: string): string {
  switch (status) {
    case 'running':
      return t.color.statusGood
    case 'starting':
    case 'unknown':
      return t.color.statusWarn
    case 'failed':
    case 'absent':
      return t.color.statusCritical
    default:
      return t.color.muted
  }
}
