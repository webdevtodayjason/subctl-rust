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

## Manual smoke checklist

Run these in a real interactive terminal (iTerm2, Terminal.app, etc.) —
the Ink renderer needs a TTY for raw-mode keyboard input. Piping into
`< /dev/null` or `script -q` is not a substitute.

1. `cd /Users/sem/code/subctl-rust-ink-tui && bin/subctl`
2. Banner appears with `daemon: running v<version>` once /health responds
3. Status line shows `◆ running … │ backend: … │ session: — │ msgs: 0`
4. Type `/help` + Enter → command list appears
5. Type `hi say two words` + Enter
   - Skill scan spinner appears immediately (single line, no flood)
   - Tokens stream in, blinking caret at the end
   - Turn commits, `└─ Response` separator appears only if ≤5 skills landed
6. Type another message → `───` separator appears above the new turn
7. Type `/sessions` + Enter → daemon's session list prints
8. Type `/skills` + Enter → first 20 skills + "+N more"
9. Type `/status` + Enter → daemon health output
10. Type `/quit` + Enter → TUI exits cleanly (no goodbye glitch)
11. Relaunch, type `/clear` → resets, session: — again
12. Ctrl+C with empty buffer → exits; with text → clears draft;
    mid-stream → interrupts
13. Enter pressed mid-stream → NO second turn fires; the buffer keeps
    growing for the next submit (verify by typing during a slow reply)
14. Heavy skill-flood: send a fresh message → spinner shows once, no
    "loaded N skill / loaded N+1 skill" churn

## Layout (planned)

Mirrors Hermes's `ui-tui` — see
`/Users/sem/code/subctl-rust-hermes-uitui/docs/hermes-uitui-spec.md` for
the citation-backed design contract this port targets.

- Two-pane: scrollback (flex-grow) / composer (flex-shrink-0)
- Per-turn `───` separator between user turns (spec §12 pattern #1)
- `└─ Response` between assistant meta block and reply (spec §12 pattern #2)
- Tree-stem tool trail (spec §12 pattern #3) — Tier 2
- Token-by-token SSE rendering with prefix-cache invariant (spec §4)
