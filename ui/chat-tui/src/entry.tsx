// Bootstrap entry — parses argv, then either prints --help/--version or
// mounts the Ink app. Kept tiny on purpose; all real work happens in
// ./app.tsx and the modules under ./{chat,input,gateway,daemon,state}.

import { render } from 'ink'
import React from 'react'

import { App } from './app.js'
import { VERSION, USAGE } from './constants.js'

function main(argv: readonly string[]): void {
  const args = argv.slice(2)

  for (const a of args) {
    if (a === '--help' || a === '-h') {
      process.stdout.write(USAGE + '\n')
      process.exit(0)
    }
    if (a === '--version' || a === '-V') {
      process.stdout.write(`subctl-chat-tui ${VERSION}\n`)
      process.exit(0)
    }
  }

  // The "chat" subcommand is the default; allow `subctl chat` as an alias.
  // Anything else falls through to the chat surface for now (subctl's other
  // subcommands are handled by the shell wrapper, not the TUI).
  if (args[0] && args[0] !== 'chat') {
    process.stderr.write(
      `[subctl] unknown argument: ${args[0]} (try --help)\n`
    )
    process.exit(2)
  }

  const inst = render(<App />, {
    exitOnCtrlC: false,
    patchConsole: false
  })

  // Ensure clean exit if Ink unmounts itself (e.g. /quit).
  inst
    .waitUntilExit()
    .then(() => process.exit(0))
    .catch((err) => {
      process.stderr.write(`[subctl] fatal: ${String(err)}\n`)
      process.exit(1)
    })
}

main(process.argv)
