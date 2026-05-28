// Top-level App component. Real chat surface, gateway wiring, slash-command
// handling, and daemon autostart land in follow-up commits. This stub is
// enough to render a banner + composer-shaped placeholder so the bundle
// runs end-to-end before the gateway lands.

import { Box, Text, useApp, useInput } from 'ink'
import React from 'react'

import { DEFAULT_THEME } from './theme.js'

export function App(): React.ReactElement {
  const { exit } = useApp()
  const t = DEFAULT_THEME

  useInput((input, key) => {
    if (key.escape || (key.ctrl && input === 'c')) {
      exit()
    }
  })

  return (
    <Box flexDirection="column" paddingX={1}>
      <Text color={t.color.primary} bold>
        {t.brand.icon} {t.brand.name}
      </Text>
      <Text color={t.color.muted}>{t.brand.welcome}</Text>
      <Box marginTop={1}>
        <Text color={t.color.prompt} bold>
          {t.brand.prompt}{' '}
        </Text>
        <Text color={t.color.muted} dimColor>
          (chat surface lands in the next commit — press Esc to exit)
        </Text>
      </Box>
    </Box>
  )
}
