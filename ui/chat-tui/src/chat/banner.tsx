// Short startup banner per spec §10. Hermes ships a 6-row ASCII logo +
// caduceus art at full width; we render a single bold line with a
// secondary tagline at any width, then break to the welcome / status row.

import { Box, Text } from 'ink'
import React from 'react'

import type { Theme } from '../theme.js'

interface BannerProps {
  theme: Theme
  daemonStatus: string
  daemonVersion: string | null
  daemonDetail: string | null
}

export function Banner(props: BannerProps): React.ReactElement {
  const { theme: t, daemonStatus, daemonVersion, daemonDetail } = props
  return (
    <Box flexDirection="column" marginBottom={1}>
      <Box>
        <Text color={t.color.primary} bold>
          {t.brand.icon} {t.brand.name}
        </Text>
        <Text color={t.color.muted} dimColor>
          {'  ·  '}
        </Text>
        <Text color={t.color.muted}>v4 chat — subctl</Text>
      </Box>
      <Text color={t.color.muted} dimColor>
        {t.brand.welcome}
      </Text>
      <Box marginTop={1}>
        <Text color={t.color.border}>─── </Text>
        <Text color={statusColor(t, daemonStatus)}>
          daemon: {daemonStatus}
        </Text>
        {daemonVersion ? (
          <Text color={t.color.muted} dimColor>
            {`  ·  v${daemonVersion}`}
          </Text>
        ) : null}
        {daemonDetail ? (
          <Text color={t.color.error}>{`  ·  ${daemonDetail}`}</Text>
        ) : null}
      </Box>
    </Box>
  )
}

function statusColor(t: Theme, status: string): string {
  switch (status) {
    case 'running':
      return t.color.ok
    case 'starting':
    case 'unknown':
      return t.color.warn
    case 'failed':
    case 'absent':
      return t.color.error
    default:
      return t.color.muted
  }
}
