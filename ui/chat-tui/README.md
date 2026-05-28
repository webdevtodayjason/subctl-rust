# @subctl/chat-tui

Ink TUI for chatting with the v4 Evy daemon. The `subctl` CLI's default
behaviour is to drop the operator into this surface.

## Stack

- Node 22 + TypeScript + React 19 + Ink 6
- `nanostores` for atomic state
- esbuild → single-file bundle at `dist/bundle.js`
- vitest for unit tests

## Build

```bash
npm install
npm run build      # → dist/bundle.js
npm test           # vitest
```

## Run (dev)

```bash
node dist/bundle.js               # launch chat surface
node dist/bundle.js --help        # usage
node dist/bundle.js --version
```

Production entry point: `bin/subctl` at the workspace root. It bootstraps
`node <bundle>` so the operator never has to know Node is in the loop.

## Daemon

Connects to `http://127.0.0.1:8797` (overridable via `EVY_HTTP_URL`). If
the daemon isn't loaded, the TUI auto-loads it via
`launchctl load ~/Library/LaunchAgents/com.subctl.evy-v4.plist` and polls
`/health` for up to 5 seconds.

## Layout (planned)

Mirrors Hermes's `ui-tui` — see
`/Users/sem/code/subctl-rust-hermes-uitui/docs/hermes-uitui-spec.md` for
the citation-backed design contract this port targets.

- Two-pane: scrollback (flex-grow) / composer (flex-shrink-0)
- Per-turn `───` separator between user turns (spec §12 pattern #1)
- `└─ Response` between assistant meta block and reply (spec §12 pattern #2)
- Tree-stem tool trail (spec §12 pattern #3) — Tier 2
- Token-by-token SSE rendering with prefix-cache invariant (spec §4)
