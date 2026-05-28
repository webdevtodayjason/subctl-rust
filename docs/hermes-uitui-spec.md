# Hermes ui-tui — comprehensive design spec

**Researched by:** hermes-uitui-deep-researcher
**Date:** 2026-05-28
**Source:** /Users/sem/code/hermes-agent/ui-tui (commit e8955f222cecb6ed7ac3f0c541b9b5b02d22843f)
**Total LOC studied:** 63,724 across ~290 `.ts`/`.tsx` files. 52,156 LOC of non-test source (the prior worker's "~9 files" estimate was off by ~32×).

This spec is the design contract for porting Hermes's chat surface to v4 Evy. Every claim is cited as `path/to/file.tsx:LINE`. Paths are rooted at `hermes-agent/ui-tui/`.

---

## 0. Stack summary

- **React 19.2.4** — strict-mode-safe, used heavily for `memo` + selective re-render via nanostores subscribers (`src/components/streamingAssistant.tsx:16`, `src/components/messageLine.tsx:29`). `useSyncExternalStore` is used directly for per-key turn-store selectors (`src/app/turnStore.ts:31`).
- **Ink ^6.8.0** — React renderer for terminals. **Stock Ink is a peerDep**; the actually-used surface is the vendored fork `@hermes/ink` (`package.json:19`, file: protocol to `packages/hermes-ink/`).
- **@hermes/ink fork** — vendored under `packages/hermes-ink/`. 924 LOC parser (`parse-keypress.ts`), 1590 LOC screen diff/optimizer (`ink/screen.ts`), 1143 LOC selection model (`ink/selection.ts`), a hand-ported Yoga shim (`native-ts/yoga-layout/index.ts`, 2326 LOC), DEC/CSI/OSC tokenizers (`termio/`), and the major React components `Box`, `Text`, `ScrollBox`, `Ansi`, `RawAnsi`, `NoSelect`, `Link`, `AlternateScreen`. Surface enumerated in `packages/hermes-ink/index.d.ts:1` and `packages/hermes-ink/src/entry-exports.ts:1`.
  - Things stock Ink does NOT have that Hermes added: `ScrollBox` with sticky-bottom, native scrollbar, viewport hooks, and pendingDelta accounting (`ScrollBoxHandle` interface at `src/types/hermes-ink.d.ts:79`); a hardware-cursor declaration API (`useDeclaredCursor` + `useCursorAdvance`, `src/types/hermes-ink.d.ts:162-167`); first-class text selection model with mouse drag + OSC52 clipboard (`useSelection`, `useHasSelection`, `src/types/hermes-ink.d.ts:144-158`); `AlternateScreen` with mouse-tracking modes (`AlternateScreen` + `MouseTrackingMode` `'off' | 'wheel' | 'buttons' | 'all'`); `NoSelect` boxes that opt rows out of the selection grid; `Ansi` and `RawAnsi` components that re-tokenize ANSI input through the renderer cleanly; `forceRedraw()` and `evictInkCaches()` operational hooks; a `Link` component with hyperlink-hover state and OSC8 emission; cursor-drift-aware fast-echo paths exposed via `useCursorAdvance`.
- **nanostores ^1.2.0** — three feature-scoped atoms, each with `patchX/resetX` helpers; React reads via `@nanostores/react` `useStore`. The atoms are intentionally NOT one global store — each one's reset/lifecycle is independent:
  - `$uiState` (`src/app/uiStore.ts:34`) — theme, sid, status text, busy flag, usage, indicator style, statusBar position, mouseTracking, sections visibility.
  - `$turnState` (`src/app/turnStore.ts:25`) — per-turn streaming buffer, reasoning, tool list, subagent list, todos, activity. Replaced wholesale every turn.
  - `$overlayState` (`src/app/overlayStore.ts:20`) — every modal/picker that can blanket the composer: approval, clarify, confirm, sudo, secret, sessions, picker, modelPicker, skillsHub, agents, pager.
  - Smaller atoms exist for cross-cutting state: `$delegationState` (`src/app/delegationStore.ts:22`), `$overlaySectionsOpen` (`src/app/delegationStore.ts:41`), `$spawnHistory`/`$spawnDiff` (`src/app/spawnHistoryStore.ts`), `$fpsState` (`src/lib/fpsStore.ts`).
- **unicode-animations ^1.0.3** — supplies branded braille spinners (`braille`, `helix`, `breathe`, `orbit`, `dna`, `waverows`, `snake`, `pulse`, `cascade`, `scan`, `diagswipe`, `fillsweep`, `rain`, `columns`, `sparkle`, `pulse`) used for thinking/tool indicators and the load banner (`src/components/thinking.tsx:41-42`, `src/components/branding.tsx:14`).
- **ink-text-input ^6.0.0** — re-exported for masked-prompt use only; the main composer is Hermes's own `src/components/textInput.tsx` (1,312 lines).
- **Streaming model** — a long-lived **JSON-over-WebSocket** session opened to a Python gateway child process spawned by the TUI (`src/gatewayClient.ts:132-156`, `src/entry.tsx:34-36`). Two URLs supported: `HERMES_TUI_GATEWAY_URL` for attach mode, `HERMES_TUI_SIDECAR_URL` for a parallel sidecar (`src/gatewayClient.ts:32-42`). Both query-strip + user-info-redact tokens before logging (`src/gatewayClient.ts:102-122`). Events arrive as a discriminated union; deltas batch at 16ms (`src/config/timing.ts:1` `STREAM_BATCH_MS`).

The total studied source files include 290+ `.ts`/`.tsx`, with the four largest non-test files being `ink.tsx` (2705), `screen.ts` (1590), `render-node-to-output.ts` (1582), and `textInput.tsx` (1312).

---

## 1. Top-level layout

ASCII mockup of the full screen at rest (no overlays, mid-turn, status bar top):

```
┌────────────────────────────────────────────────────────────────────┐
│ ─ ready  │  claude opus 4.7  │  18.2k/200k  │  [██████░░░░] 60%  ─ ~/code  │   ← StatusRule (top)
│                                                                              │
│  ⚕ Hermes Agent                                                            │   ← Banner (intro msg)
│  ────────────────────────────────────────────────────────                 │
│  Nous Research · Messenger of the Digital Gods                              │
│                                                                              │
│  ╭───────── Hermes Agent v1.2 ─────────╮                                   │
│  │   ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⡀⠀⣀⣀⠀⢀⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀                              │   ← SessionPanel
│  │   (caduceus art) opus-4-7 · Nous Research                              │      (hero + tools/skills accordion)
│  │   ▾ Available Tools                                                    │
│  │   ▸ Available Skills (47)  in 11 categories                            │
│  │   ▸ System Prompt    — 8,200 chars                                    │
│  │   ▸ MCP Servers (3)  connected                                        │
│  │   24 tools · 47 skills · 3 MCP · /help for commands                   │
│  ╰─────────────────────────────────────╯                                  │
│                                                                            │
│  ❯  refactor the auth module to use the new session interface           │   ← user msg
│                                                                          │
│  ┊  I'll start by reading the current auth flow.                        │   ← assistant msg
│     ├─ ▾ Thinking  ~2.1k tokens                                        │      tool trail (collapsed)
│     │   │ Looking at the auth.ts file…                                  │
│     │   │ The current pattern uses callbacks, but…                      │
│     ├─ ▾ Tool calls (3)                                                 │
│     │   ● read_file  · auth.ts (1.2s)                                  │
│     │   ● read_file  · session.ts (0.8s)                               │
│     │   └─ ● grep_files · "auth\\." (12 matches, 0.4s)                  │
│     └─ Σ ~3.4k total                                                    │
│  ┊  Here's what I found:                                               │      response separator
│  └─ Response                                                            │
│  ┊  The current auth flow does three things:                            │
│     1. Validates the session token                                      │
│     2. Loads the user profile                                           │
│     3. Refreshes the access token if needed                             │
│                                                                            │
│  ───                                                                       │   ← second-user turn dash
│  ❯  proceed with the refactor                                             │
│                                                                            │
│  (◔_◔) pondering…   ⠋                                                   │   ← live status (FaceTicker)
│                                                                            │
│  queued (2) · editing 1 · Ctrl+X delete · Esc cancel                       │   ← QueuedMessages
│  ▸ 1. add tests for the new flow                                          │
│    2. update the changelog                                                │
│                                                                            │
│  ↳ refactor the auth module to use…                                       │   ← StickyPromptTracker
│  ❯ |                                                                       │   ← composer prompt + cursor
└────────────────────────────────────────────────────────────────────┘
        ▲
        └─ TranscriptScrollbar (1-col gutter, sticky thumb)
```

- **`AlternateScreen` wrap** with `mouseTracking` prop driving the DEC mouse mode (`src/components/appLayout.tsx:393-397`). When `HERMES_TUI_INLINE=1` (or Termux default) the wrapper is `React.Fragment` and the TUI renders into the host terminal's primary scrollback (`src/config/env.ts:52-60`).
- **Root flexbox**: `<Box flexDirection="column" flexGrow={1}>` containing a row that flexes to fill (transcript or full-screen agents overlay), followed by fixed-shrink rows for PromptZone + ComposerPane + optional FpsOverlay (`src/components/appLayout.tsx:396-433`).
- **Transcript pane** (`src/components/appLayout.tsx:56-162` `TranscriptPane`):
  - `<ScrollBox stickyScroll>` flex-grows to take all space above the composer; the optional 1-col `TranscriptScrollbar` is wrapped in `<NoSelect>` so selection-drag never includes the gutter (`src/components/appLayout.tsx:150-152`).
  - Inside the ScrollBox, a column `<Box paddingX={1}>` holds virtualised rows. `transcript.virtualHistory.topSpacer` / `bottomSpacer` heights replace off-screen rows so total scroll height stays accurate (`src/components/appLayout.tsx:103-137`).
  - The `intro` msg row renders the responsive banner + `SessionPanel` (`src/components/appLayout.tsx:113-118`).
  - The `panel` msg kind renders a centered titled `Panel` (`src/components/appLayout.tsx:119-120`, definition `src/components/branding.tsx:418-453`).
  - Every other msg renders via `<MessageLine>` (`src/components/appLayout.tsx:122-130`).
  - Live `<LiveTodoPanel>` is mounted as a child of the *latest* user message row so it visually belongs to that prompt and scrolls with it (`src/components/appLayout.tsx:67-77, 133`). Once that turn ends, the todo gets archived into the transcript as a `kind:'trail'` msg (`src/app/turnStore.ts:43-65`).
  - Every user message after the first is preceded by a one-cell margin and a `───` border-coloured separator (`src/components/appLayout.tsx:107-111`). This is the *only* turn-boundary affordance; there is no other "turn N" framing.
- **Streaming area** appears at the bottom of the scroll, after virtual rows. `<StreamingAssistant>` renders three things in order: any committed `streamSegments` (slash output, diffs, narration broken by tool calls), the active `tools[]` shelf, the in-flight assistant text (`src/components/streamingAssistant.tsx:36-91`).
- **PromptZone** is a flex-shrink-0 region above the composer that hosts the modal-style approval/clarify/confirm/sudo/secret prompts inline (not floating). When no prompt is active it returns null (`src/components/appOverlays.tsx:20-93`).
- **ComposerPane** (`src/components/appLayout.tsx:164-326`): a `<NoSelect flexDirection="column" flexShrink={0} paddingX={1}>` with five stacked rows:
  1. `<QueuedMessages>` (rendered iff queue non-empty).
  2. Background-task count line (iff `bgTasks.size > 0`).
  3. The "sticky prompt" recap line OR a 1-row blank spacer used as a drag-handle for selection (`src/components/appLayout.tsx:238-246`).
  4. The actual prompt-prefix + `TextInput` body. `inputBuf` (committed lines from multi-line paste) renders above the live `<TextInput>` row.
  5. The "summoning hermes…" line iff a turn is mid-flight without a session yet (`src/components/appLayout.tsx:321`).
- **StatusRule** (`src/components/appChrome.tsx:294-424`) is the bottom-line of the layout: 1-row `<Box>` with a left flex-shrink section (status text or FaceTicker + model + context + spawn HUD + voice + sessions + bg count + cost) and a right flex-shrink-0 section showing cwd. `─` border separators visually mark sections (`src/components/appChrome.tsx:343-345, 414`). Position is `'top' | 'bottom' | 'off'`, defaults to `'top'`, toggled by `/statusbar` (`src/app/uiStore.ts:29`, `src/app/slash/commands/core.ts:522-547`).
- **TranscriptScrollbar** (`src/components/appChrome.tsx:451-522`): right-edge 1-col gutter. Track uses `│` characters in `muted` (or `border` on hover); thumb uses `┃` in `border` (default) → `accent` (hover) → `primary` (grabbed). Click-to-jump and drag both supported with `mouseAnchor` offset preserved (`src/components/appChrome.tsx:469-497`).
- **FpsOverlay** is right-justified and only mounted when `HERMES_TUI_FPS=1` (`src/components/fpsOverlay.tsx:13-19`, `src/config/env.ts:64`).

---

## 2. Color & typography

The theme is split into `ThemeColors`, `ThemeBrand`, and banner art (`src/theme.ts:1-52`). Two stock themes shipped:

### Dark theme (default) — `src/theme.ts:257-305`

| Role | Hex | Comment |
|---|---|---|
| `primary` | `#FFD700` | banner title gold |
| `accent` | `#FFBF00` | banner secondary + ▸/▾ chevrons |
| `border` | `#CD7F32` | bronze for box borders + tool stem |
| `text` | `#FFF8DC` | body cornsilk-on-dark |
| `muted` | `#CC9B1F` | secondary copy. Bumped from `#B8860B` for legibility (`src/theme.ts:262-268`) |
| `label` | `#DAA520` | user-prompt color, field labels |
| `ok` / `error` / `warn` | `#4caf50` / `#ef5350` / `#ffa726` | status semantics |
| `prompt` | `#FFF8DC` | active composer prompt glyph |
| `sessionLabel` / `sessionBorder` | `#CC9B1F` | track muted by design |
| `statusBg` / `statusFg` | `#1a1a2e` / `#C0C0C0` | only used by bar |
| `statusGood` / `statusWarn` / `statusBad` / `statusCritical` | `#8FBC8F` / `#FFD700` / `#FF8C00` / `#FF6B6B` | context-bar tiers |
| `selectionBg` | `#3a3a55` | text-selection backing |
| `diffAdded` / `diffRemoved` | `rgb(220,255,220)` / `rgb(255,220,220)` | inline diff backgrounds |
| `diffAddedWord` / `diffRemovedWord` | `rgb(36,138,61)` / `rgb(207,34,46)` | inline diff fg |
| `shellDollar` | `#4dabf7` | `$` glyph when user input starts with `!` |
| `completionBg` / `completionCurrentBg` | `#1a1a2e` / `#333355` | autocomplete row |

### Light theme — `src/theme.ts:310-350`

A parallel palette with darker golds/ambers (`#8B6914`, `#A0651C`, `#7A4F1F`) and white-friendly diff swatches. Light-mode detection layers five signals (`src/theme.ts:415-475`):

1. `HERMES_TUI_LIGHT` boolean → wins.
2. `HERMES_TUI_THEME=light|dark` named → wins next.
3. `HERMES_TUI_BACKGROUND` hex hint, luminance ≥ 0.6 → light.
4. `COLORFGBG` last field 7 or 15 → light; 0–14 → authoritatively dark.
5. `TERM_PROGRAM` allow-list (`Apple_Terminal` only) → light.

### ANSI fallback for non-truecolor Apple Terminal

When light mode + Apple Terminal + not truecolor, foregrounds are normalised to `ansi256(N)` via a luminance-weighted selector (`src/theme.ts:477-504`). The selector caps at 0.72 luminance to avoid washing out on white backgrounds and adds a saturation floor penalty (`src/theme.ts:202-215`).

### Skin override — `src/theme.ts:516-589`

`gateway.ready` and `skin.changed` events can ship a `GatewaySkin` payload (`src/gatewayTypes.ts:3-10`) carrying `colors`, `branding`, `banner_logo`, `banner_hero`, `tool_prefix`, `help_header`. `fromSkin()` layers them on top of the default palette, mixing custom completion-current-bg from `completion_menu_bg × bannerAccent @ 0.25` when only one of the pair is overridden (`src/theme.ts:533-535`).

### Typography conventions

- **Bold**: assistant `Response` separator label is *not* bold (it's `muted dim`, `src/components/messageLine.tsx:205-207`). User prompt glyph IS bold (`src/components/messageLine.tsx:213-214`). Heading rows in panels/MD are bold + accent (`src/components/markdown.tsx:878-881`, `src/components/branding.tsx:78`).
- **Italic** is reserved for inline math (`src/components/markdown.tsx:593-596`) and the `*italic*` markdown token (`markdown.tsx:546-550`). Nothing else uses italic.
- **Underline** is only used for links (`src/components/markdown.tsx:178-181`, `markdown.tsx:707-710`).
- **Strikethrough** for `~~strike~~` MD.
- **Inverse** for selected rows in pickers (e.g. `src/components/sessionPicker.tsx:183-184`, `modelPicker.tsx:425-433`), for the text-input cursor (`src/components/textInput.tsx:40, 353-372`), and for `\boxed{X}` math regions (`src/components/markdown.tsx:46-52`).
- **`dim`** widely used for chrome that should fade behind primary content: tree stems, separators, "Response" label.

### Indicator styles

`/indicator [kaomoji|emoji|unicode|ascii]` swaps the busy ticker:

- `kaomoji` (default) — rotates through `FACES` array every 2500ms (`src/content/faces.ts:1-17`, `src/components/appChrome.tsx:20, 47-49`). 15 kaomoji.
- `emoji` — rotates `⚕ 🌀 🤔 ✨ 🍵 🔮` every 600ms (`src/components/appChrome.tsx:30, 52-58`).
- `ascii` — `| / - \` spinner at 100ms (`src/components/appChrome.tsx:31, 60-66`).
- `unicode` — braille spinner from `unicode-animations`, ~80ms; this style hides the rotating verb suffix entirely (`src/components/appChrome.tsx:72-76`).

The verb is appended as `${verb}…` padded to the widest VERB so the layout doesn't jitter (`src/components/appChrome.tsx:22-26`). VERBS = `pondering`, `contemplating`, `musing`, `cogitating`, `ruminating`, `deliberating`, `mulling`, `reflecting`, `processing`, `reasoning`, `analyzing`, `computing`, `synthesizing`, `formulating`, `brainstorming` (`src/content/verbs.ts:22-38`).

---

## 3. Message rendering

### Data shape

`Msg` (`src/types.ts:112-125`):

```ts
interface Msg {
  role: 'assistant' | 'system' | 'tool' | 'user'
  text: string
  kind?: 'diff' | 'intro' | 'panel' | 'slash' | 'trail'
  thinking?: string
  thinkingTokens?: number
  toolTokens?: number
  tools?: string[]           // pre-formatted tool-trail lines
  todos?: TodoItem[]
  todoIncomplete?: boolean
  todoCollapsedByDefault?: boolean
  info?: SessionInfo         // for kind: 'intro'
  panelData?: PanelData      // for kind: 'panel'
}
```

`SessionInfo` carries model, profile_name, reasoning_effort, fast/service_tier, skills (`Record<string, string[]>`), tools (same shape), mcp_servers, update_behind, usage, system_prompt (`src/types.ts:146-163`).

### Role-glyph table

`ROLE` map (`src/domain/roles.ts:4-9`):

| Role | Glyph | Body color | Prefix color |
|---|---|---|---|
| `user` | `t.brand.prompt` (default `❯`, bold) | `t.color.label` (gold) | `t.color.label` |
| `assistant` | `t.brand.tool` (default `┊`) | `t.color.text` | `t.color.border` |
| `tool` | `⚡` | `t.color.muted` | `t.color.muted` |
| `system` | `·` | (default — `''`) | `t.color.muted` |

Glyph + 1-space gutter goes on the left; body wraps inside `transcriptBodyWidth(cols, role, prompt, termux)` (`src/components/messageLine.tsx:107, 218`). `<NoSelect>` wraps the gutter so click-drag never selects the role glyph (`src/components/messageLine.tsx:202-216`).

### Per-kind rules

- **`kind:'trail'` + todos** → renders only a `<TodoPanel>` (`src/components/messageLine.tsx:56-65`).
- **`kind:'trail'` with tools/thinking** → renders only a `<ToolTrail>` (`src/components/messageLine.tsx:67-82`). Trail messages have empty `text`.
- **`role:'tool'`** → renders as a single-row rounded-border `<Box>` with the tool result preview, ANSI-aware. ANSI-bearing text is sanitised + truecolor-stripped before pass-through to `<Ansi>` (`src/components/messageLine.tsx:85-104`).
- **`kind:'slash'`** → renders body in `muted` color, no markdown processing (`src/components/messageLine.tsx:115-117`).
- **`role:'system'` + length > 400 chars** → collapses behind a `▸ first-line — N chars` chevron (`src/components/messageLine.tsx:122-138`).
- **`role:'assistant'`** → markdown render via `<Md>` for committed text, or `<StreamingMd>` while `isStreaming` (`src/components/messageLine.tsx:144-155`). Bounded by `boundedLiveRenderText()` to cap 16,000 chars / 240 lines while streaming (`src/config/limits.ts:3-4`).
- **`role:'user'` + long + pasted-backed text** → renders `[long message]` placeholder dim, leaving prose context (`src/components/messageLine.tsx:157-169`).
- **`kind:'diff'`** → margin-1 above and below; the diff itself is in the text (`src/components/messageLine.tsx:177, 182-184`).

### Tool-trail line shape

Tools persisted from previous turns come back as preformatted strings parsed by `parseToolTrailResultLine` and `formatToolCall`. The active-turn live shelf gets formatted via `formatToolCall(tool.name, tool.context)` and rendered with a streaming spinner (`src/components/thinking.tsx:852-876`). Tool result lines look like `read_file · auth.ts (1.2s)` with the duration as a `statusFg dim` suffix (`src/components/thinking.tsx:904-918`).

### Response separator

When an assistant message has any visible `thinking` or `tools[]` content AND has any text body, a `└─ Response` separator inserts above the body text (`src/components/messageLine.tsx:112, 200-209, 224-225`). The separator is gutter-aligned via `<NoSelect width={gutterWidth}>` and renders as `└─ ` border / `Response` muted+dim. This is the visual cue that says "the in-channel reply starts here, after the meta block above".

### Markdown rendering — `src/components/markdown.tsx`

Full Markdown subset implementation (no external library). The `<Md>` component (1119 lines) handles:

- **Block parsers** dispatched in priority order: fenced code (` ``` ` / `~~~`), display math (`$$…$$`, `\[…\]`), headings (`#…######` + setext), HR (`---`/`***`/`___`), footnotes (`[^N]:`), definition lists (`Term\n: def`), bullets (`-`/`+`/`*` with `[ ]/[x]` task checkboxes), numbered lists, blockquotes (`>` w/ depth), tables (pipe + divider row), `<details>`/`<summary>` HTML.
- **Inline regex** in priority order (`src/components/markdown.tsx:108-132`): image, link, autolink, strike, code, bold-`**`, bold-`__`, italic-`*`, italic-`_`, highlight-`==`, footnote ref, superscript `^X^`, subscript `~X~` (alphanumeric ≤8), bare URL, inline math `$x$`, inline math `\(x\)`.
- **Tables** get a three-tier layout (ideal widths, proportional shrink, hard-break with vertical fallback) and a grapheme-safe word wrap using `Intl.Segmenter`. Header row renders bold-accent, separator dim-muted (`src/components/markdown.tsx:214-494`).
- **Code blocks** with language detection. Highlighter (`src/lib/syntax.ts`) supports `go`, `json`, `py`/`python`, `rust`/`rs`, `sh`/`bash`/`shell`/`zsh`, `sql`, `ts`/`tsx`/`js`/`jsx`/`javascript`/`typescript`, `yaml`/`yml`. Keywords colored `t.color.border`, strings `t.color.accent`, comments `t.color.muted`, numbers `t.color.text`. Diff blocks render with `diffAdded`/`diffRemoved` backgrounds + `diffAddedWord`/`diffRemovedWord` fg + dim context lines (`src/components/markdown.tsx:751-790`).
- **Math** via `texToUnicode` (`src/lib/mathUnicode.ts`, 770 LOC) translates LaTeX commands into Unicode; `\boxed{}` regions render `bold inverse` over a 1-cell pad. Block math gets a `paddingLeft={2}` column (`src/components/markdown.tsx:813, 859-867`).
- **Markdown cache**: theme-keyed WeakMap of insertion-ordered Map LRU sized at 512 entries. Keyed by `compact|cols|text` (`src/components/markdown.tsx:613-646`). Critical for virtualised re-rendering — a row that scrolls back into view doesn't re-tokenize.
- **External links** carry through `useLinkTitle` (`src/lib/externalLink.ts`), which can asynchronously fetch the document title and show that as the label (`src/components/markdown.tsx:166-183`). Display falls back to a URL slug or the raw URL.
- **MEDIA: directives** (`MEDIA: <path>`) render as a `▸` linked row pointing at `file://` for absolute paths or the literal URL (`src/components/markdown.tsx:699-713`).

### Streaming markdown — `src/components/streamingMarkdown.tsx`

Key insight: naive `<Md text={full}>` re-tokenises everything on every delta. `<StreamingMd>` splits the text at the last *stable* `\n\n` boundary outside fenced code or display-math blocks, then renders two `<Md>` siblings in a column. The stable prefix grows monotonically via a `useRef`, so its memo key stays identical → React reuses the cached subtree. Only the unstable suffix re-parses per delta (`src/components/streamingMarkdown.tsx:1-167`).

The fence balance scan (`fenceOpenAt`) is intentionally *more* conservative than the renderer's parser: any unmatched `$$` opener is treated as still-open so a half-streamed math block doesn't get committed as paragraph prose (`src/components/streamingMarkdown.tsx:42-102`).

The two-Md children MUST be wrapped in `<Box flexDirection="column">` — Ink's default `row` direction would lay them side-by-side and cause the "two jumbled columns while streaming" rendering bug noted at `streamingMarkdown.tsx:24-29`.

---

## 4. Streaming behavior

- **Token granularity** — `message.delta` events ship either `text` or `rendered` fragments (`src/gatewayTypes.ts:556`). The gateway batches at the wire level; the UI batches another 16ms layer in `turnController` (`src/config/timing.ts:1`).
- **In-flight cursor** — a `▍` blinking caret toggles at 420ms intervals at the end of the active-streaming reasoning/text (`src/components/thinking.tsx:194-230`). The cursor is gated on `streaming && visible`; when not visible it renders a single space so the line height doesn't shift.
- **Active-tool spinner** — each running tool gets a per-tool braille spinner picked from `TOOL` (`cascade`, `scan`, `diagswipe`, `fillsweep`, `rain`, `columns`, `sparkle`) at its authored interval (`src/components/thinking.tsx:42, 155-174`). Elapsed seconds tick at 500ms (`src/components/thinking.tsx:751-754`).
- **Stream error mid-message** — `error` events dispatch via the global handler, surface a system message; the partial assistant text stays in `streaming` until `message.complete` arrives or a new turn starts (`src/app/createGatewayEventHandler.ts:556-562`).
- **Skill-loaded / context-injected metadata** — surfaced via the intro `SessionPanel.Available Skills` accordion (`src/components/branding.tsx:194-220`). Skill installs flagged through `skills.manage` RPC are handled by the `SkillsHub` overlay (`src/components/skillsHub.tsx`), not inline narration.
- **"Compacting context"** — surfaced via the `/compress` slash command and `cmp N` segment in StatusRule. When `usage.compressions ≥ 5` → warn color, `≥10` → error (`src/components/appChrome.tsx:375-384`).
- **Thinking status pulse** — when reasoning is active and no other status is preempted, status text rotates among schedule-set values (e.g. specific "thinking…" lines from `thinking.delta` events) via `scheduleThinkingStatus` at 16ms grain (`src/app/createGatewayEventHandler.ts:150-161`). Polished to read like the agent is keeping the user informed in real time.
- **Status escalation pattern** — `setStatus()` overrides the pulse immediately; `restoreStatusAfter(ms)` returns to default `busy ? 'running…' : 'ready'` after a delay (`src/app/createGatewayEventHandler.ts:139-169`).
- **`reasoning.delta` / `reasoning.available`** events feed live thinking content; rendered with `boundedLiveRenderText` (cap 16k/240) and a blinking cursor (`src/components/thinking.tsx:619-677`).
- **Streaming todos**: live `<LiveTodoPanel>` mounted as a child of the latest user msg row so it stays anchored to the prompt that spawned it (`src/components/appLayout.tsx:67-77`).

### "Good Vibes Heart" easter egg

If a user submission matches `\b(good bot|thanks|thank you|thx|ty|ily|love you)\b` (case-insensitive, `src/app/useMainApp.ts:47`), a `♥` flickers in the composer's top-right corner for 650ms in a randomly-picked color from `[error, warn, accent]` (`src/components/appChrome.tsx:269-292`). It's a tiny, deliberate moment of personality.

---

## 5. Input box — `src/components/textInput.tsx` (1,312 LOC)

This is the most complex single component in the codebase. Highlights:

### Geometry

- Multi-line by default; the visible cursor is positioned by `cursorLayout()` which delegates wrap math to `wrap-ansi` to guarantee the hardware cursor never desyncs from rendered cells (`src/components/textInput.tsx:474, 480`, `src/lib/inputMetrics.ts:35-66`).
- `inputBuf` array (committed lines from multi-line paste) renders ABOVE the active TextInput row (`src/components/appLayout.tsx:267-281`).
- Composer prompt prefix (`❯` by default, `$` in shell mode, profile-prefixed when not `default`/`custom`) renders with `COMPOSER_PROMPT_GAP_WIDTH=1` gap (`src/lib/prompt.ts:3-35`, `src/lib/inputMetrics.ts:5`).

### Submit / continuation

- **Enter** → submit.
- **Shift+Enter, Ctrl+Enter, (Cmd+Enter on macOS), Meta+Enter** → insert `\n` (`src/components/textInput.tsx:946-948`).
- **`\` + Enter** → multiline continuation fallback for terminals that swallow Shift+Enter (`src/content/hotkeys.ts:34`).

### Cursor / selection

- Grapheme-aware caret using `Intl.Segmenter` with a LRU cache of 32 strings (`src/components/textInput.tsx:43-78`).
- `prevPos`/`nextPos` and `wordLeft`/`wordRight` for arrow nav and word jumps (Alt+←/→ or modifier-arrow on platform).
- `lineNav(s, p, dir)` preserves column when arrowing up/down. Returns `null` if there's no further line so the parent can cycle history (`src/components/textInput.tsx:186-211`).
- Mouse selection: click + drag (button 0) selects a range; right-click copies the selection if there is one, otherwise pastes (`src/components/textInput.tsx:1183-1219`, `decideRightClickAction` at `:1279-1292`).
- Multi-click ≤500ms at the same offset → `selectAll()` (`src/components/textInput.tsx:861-867, 1205-1209`).

### Paste handling

- Bracketed paste detected by `\x1b[200~ ... \x1b[201~`. `\r\n` and `\r` normalised to `\n`. Any text containing `\n` is routed via `onPaste` callback so the host can render a "long paste collapse" (`src/components/textInput.tsx:1084-1108`, `shouldRouteMultiCharInputAsPaste` at `:123`).
- `onPaste` may return a Promise to defer (e.g. for clipboard image attach via gateway); during the wait, raw typing is appended to the same insert position; an `editVersionRef` guards against race conditions (`src/components/textInput.tsx:696-725`).
- Cmd+V on macOS, Alt+V (`\x1bv`/`\x1bV`), or Ctrl+V (`\x16`) all trigger the paste flow even if the host terminal doesn't bracket the paste (`src/components/textInput.tsx:892-913`).

### Fast-echo bypass

For pure ASCII printable single-line append at end-of-string under width — bypass Ink and write directly to stdout with a corresponding `useCursorAdvance()` call so Ink's display-cursor model stays in sync (`src/components/textInput.tsx:1132-1152`, `canFastAppendShape` at `:240-264`).

Mirrored backspace fast-path uses `'\b \b'` shortcut with similar gates: never at soft-wrap boundary, never with combining marks (`src/components/textInput.tsx:1025-1042`, `canFastBackspaceShape` at `:293-328`). Apple Terminal and Termux opt out (`src/components/textInput.tsx:330-351`).

### Slash command discovery

A `?`-only input shows the floating `<HelpHint>` panel above the composer (`src/components/appLayout.tsx:264`, `src/components/helpHint.tsx:17-73`). The hint lists 6 common commands + 8 hotkeys from `HOTKEYS`.

Any `/` prefix triggers slash completion via `complete.slash` RPC; results appear in the autocomplete dropdown below the composer (`src/hooks/useCompletion.ts:11-39`).

### Voice record key

Configurable via `voice.record_key` config (defaults to `Ctrl+B`, parsed by `parseVoiceRecordKey`). When that key fires, the composer ignores it and lets the global input handler route it to `voice.record` RPC (`src/components/textInput.tsx:884-890, 1294-1306`).

### Placeholder text

`PLACEHOLDER` is randomly chosen at module load from 7 strings: `Ask me anything…`, `Try "explain this codebase"`, `Try "write a test for…"`, `Try "refactor the auth module"`, `Try "/help" for commands`, `Try "fix the lint errors"`, `Try "how does the config loader work?"` (`src/content/placeholders.ts:3-13`).

In TTY mode, the placeholder renders as `dim(placeholder)` so the hardware cursor at column 0 marks the input start (`src/components/textInput.tsx:506-520`).

When the agent is busy, the placeholder becomes the literal string `'Ctrl+C to interrupt…'` (`src/components/appLayout.tsx:307`).

### Undo / redo

A bounded ring of 200 entries holds previous `{value, cursor}` pairs (`src/components/textInput.tsx:443-444, 651-660`). Ctrl+Z / Ctrl+Y bound on all platforms via `isActionMod` (`textInput.tsx:957-958, 972-978`).

### Composer-line shell mode

When `input` starts with `!`, the prompt prefix changes to `$` in `shellDollar` color (`#4dabf7`) and Enter sends the shell command through `shell.exec` RPC (`src/components/appLayout.tsx:171, 290-291`). Documented at `src/content/hotkeys.ts:35`.

`{!<cmd>}` inline interpolation expands embedded shell commands inside a normal prompt (`src/content/hotkeys.ts:36`, `src/protocol/interpolation.ts`).

---

## 6. Slash commands

Slash commands are dispatched server-side by default via `command.dispatch` RPC (`src/gatewayTypes.ts:46-50`). The TUI-local registry adds visual + UX layer on top. Local commands live in `src/app/slash/commands/`:

### `core.ts` (648 LOC) — 22 commands

| Command | Aliases | Purpose |
|---|---|---|
| `/help` | — | Render slash catalog + hotkeys as a `Panel`. |
| `/quit` | `/exit` | Tear down + exit (`session.die`). |
| `/update` | — | Exit with code 42; the wrapper re-execs `hermes update`. |
| `/mouse` | `/scroll` | Set `mouseTracking` preset (`on/off/toggle/wheel/buttons/all`). |
| `/clear` | `/new` | Confirm-gate then start a new session. |
| `/redraw` | — | `forceRedraw(process.stdout)` from `@hermes/ink`. |
| `/status` | — | Page session status via `session.status` RPC. |
| `/resume` | — | Open `SessionPicker` overlay; or accept an explicit id arg. |
| `/title` | — | Get/set the session title. |
| `/compact` | — | Toggle `ui.compact` (collapses blank gaps in markdown). |
| `/details` | `/detail` | Set global agent-detail mode `hidden/collapsed/expanded/cycle` OR per-section override. |
| `/fortune` | — | Local fortune; `daily` mode is seeded by `sid + dayString`. |
| `/copy` | — | Copy selection (clipboard), or last assistant msg (index N optional). Falls back to OSC52. |
| `/paste` | — | Attach clipboard image via `clipboard.paste` RPC. |
| `/terminal-setup` | — | Configure IDE terminal keybindings (vscode/cursor/windsurf). |
| `/logs` | — | Last N gateway log lines (`gw.getLogTail`). |
| `/history` | — | TUI's in-memory transcript (user+assistant only). |
| `/save` | — | Persist via `session.save`. |
| `/statusbar` | `/sb` | Position: `on/off/top/bottom/toggle`. |
| `/queue` | `/q` | Inspect or enqueue a message. |
| `/steer` | — | Inject a prompt that lands after the next tool call (`session.steer` RPC), without interrupting the agent. |
| `/undo` | — | `session.undo` + trim last exchange from local history. |
| `/retry` | — | `session.undo` + resend last user message. |

### `session.ts` (554 LOC) — 14 commands

| Command | Aliases | Purpose |
|---|---|---|
| `/background` | `/bg`, `/btw` | Launch a `prompt.background` task. |
| `/model` | — | Open `ModelPicker` (or accept `name --provider X` arg). Session-scoped by default, `--global` for persistent. |
| `/sessions` | `/switch` | Open `ActiveSessionSwitcher` (live-session orchestrator overlay). |
| `/image` | — | `image.attach` RPC with path arg; replaces composer input with remainder. |
| `/personality` | — | Set personality; reset transcript if backend says `history_reset`. |
| `/compress` | — | `session.compress`; replaces transcript with new compressed history. |
| `/branch` | `/fork` | Fork current session, close the previous one. |
| `/voice` | — | `voice.toggle` action `on/off/tts/status`; renders cli.py-style status block. |
| `/skin` | — | Switch skin theme (fires `skin.changed`). |
| `/indicator` | — | Pick `kaomoji/emoji/unicode/ascii` busy ticker. |
| `/yolo` | — | Per-session toggle for auto-approve. |
| `/reasoning` | — | Show/set reasoning effort; toggles `sections.thinking` accordingly. |
| `/fast` | — | `normal/fast/status/on/off/toggle`. |
| `/busy` | — | `queue/steer/interrupt/status` — what Enter does while busy. |
| `/verbose` | — | Cycle verbose tool-output mode. |
| `/usage` | — | Render cost + token panel via `session.usage`. |

### `ops.ts` (717 LOC) — operational commands

`/stop` (kill bg processes), `/reload-mcp [now|always|approve]` (with prompt-cache invalidation warning gate), `/reload` (re-read env), `/browser [connect|disconnect|status]` (CDP), `/rollback [list|diff|hash] [file]` (Git checkpoint UI), `/agents [pause|resume|status]` or open spawn-tree dashboard (`src/app/slash/commands/ops.ts:64-300+`).

### `setup.ts`

`/setup [args]` — suspends Ink via `withInkSuspended`, exec `hermes setup` interactively, return when done (`src/app/slash/commands/setup.ts:7-19`).

### `debug.ts`

`/heapdump` (V8 snapshot) and `/mem` (live heap + rss panel) (`src/app/slash/commands/debug.ts`).

### Help system

`/help` renders categories from `ctx.local.catalog` (backend-provided slash catalog), plus a hardcoded "TUI" section (`/details`, `/fortune`) and the full hotkey grid. Header text is `t.brand.helpHeader` (default `(^_^)? Commands`).

Per-command help is *not* implemented as `/help <cmd>` — instead the dropdown autocomplete shows `display | meta` rows for each candidate as you type (`src/components/appOverlays.tsx:215-251`).

---

## 7. State management (nanostores)

### `$uiState` — `src/app/uiStore.ts`

Single atom holding everything chrome-shaped: theme, session id, status string, busy flag, indicator style, statusBar position, mouseTracking, details mode, sections visibility, usage, paste-collapse thresholds, voice flags, info (SessionInfo or null). Three derived computeds: `$uiTheme`, `$uiSessionId`. Default state at `src/app/uiStore.ts:9-32`.

**Producers**: `patchUiState(next | (state) => state)` is the only setter (`uiStore.ts:41-42`). Used from slash commands, gateway event handler, config sync.

**Consumers**: `<AppLayout>`, `<StatusRule>`, `<MessageLine>` (compact + sections), `<TranscriptPane>` (theme), `<StreamingAssistant>` (theme), `<HelpHint>`, every overlay (theme), `<TextInput>` (voiceRecordKey). React subscribes via `useStore($uiState)`.

### `$turnState` — `src/app/turnStore.ts`

Per-turn ephemeral state, replaced wholesale each turn. Holds the live `streaming` text, `reasoning` text, `reasoningActive`/`reasoningStreaming`/`reasoningTokens`, `tools` array of `ActiveTool`, `subagents`, `todos`/`todoCollapsed`, `streamSegments` (assistant segments separated by tool calls), `streamPendingTools`, `turnTrail`, `activity`, `outcome`. Reset on `idle()` (`src/app/turnController.ts`).

The reset interaction with todos is interesting: `archiveTodosAtTurnEnd` converts the live `todos` into a `kind:'trail'` history message before clearing (`src/app/turnStore.ts:43-65`). Completed todos collapse-by-default; incomplete-only sets `todoIncomplete: true` which surfaces a `· incomplete · N still pending` suffix (`src/components/todoPanel.tsx:67-72`).

Subscribers use `useTurnSelector(state => state.X)` which wraps `useSyncExternalStore` for shallow-comparable slices (`src/app/turnStore.ts:31-36`).

### `$overlayState` — `src/app/overlayStore.ts`

Holds every modal/picker that can blanket the composer: `agents`, `approval`, `clarify`, `confirm`, `modelPicker`, `pager`, `picker`, `secret`, `sessions`, `skillsHub`, `sudo`. `$isBlocked` is a computed that's true if any are present (`src/app/overlayStore.ts:22-26`).

`resetFlowOverlays()` is the *soft* reset: drops approval/clarify/confirm/sudo/secret/pager (flow-scoped) but PRESERVES user-toggled ones (agents/modelPicker/picker/sessions/skillsHub) — called from `turnController.idle()` so an `/agents` panel that was open before the turn doesn't vanish when the turn ends (`src/app/overlayStore.ts:44-53`).

### `$delegationState`, `$overlaySectionsOpen`, `$spawnHistory`, `$spawnDiff`, `$fpsState`, `$inputSelection`

Small atoms for cross-cutting concerns. Notable: `$overlaySectionsOpen` lifts accordion open-state out of `<SubagentAccordion>` local state so it survives node remount as the user walks history or filters (`src/app/delegationStore.ts:31-54`).

---

## 8. Animations + visual polish

- **Banner loader** — `<InlineLoader>` in `branding.tsx:10-28` uses braille spinner at 120ms while skills/tools scan.
- **Indicator tickers** — see §2.
- **Thinking spinner** — `<Spinner variant="think|tool">` picks a random spinner from THINK/TOOL pools and animates at the spinner's authored interval (`src/components/thinking.tsx:154-174`).
- **Streaming cursor** — `<StreamCursor>` toggles `▍` at 420ms, hides when not streaming (`src/components/thinking.tsx:194-230`).
- **Good Vibes Heart** — random palette flash, 650ms (`src/components/appChrome.tsx:269-292`).
- **No fade-in transitions** — all message commits are abrupt; the streaming subtree memoization keeps the appearance smooth (the user sees text appear character-by-character, not row-by-row).
- **No screen flash on session reset** — the AlternateScreen handles boot/exit cleanly via reset terminal modes (`src/entry.tsx:23, 41-47`).
- **Banner ASCII art** — see §10.
- **Caduceus art** — 15-line braille caduceus in `src/banner.ts:55-71`. Tied to the medical / "messenger of the gods" branding.

---

## 9. Keyboard model (complete)

This list is best-effort; the canonical source is `src/app/useInputHandlers.ts` (576 LOC) plus per-overlay handlers.

### Global (composer focused, no overlay)

- **Enter** — submit input.
- **Shift+Enter / Ctrl+Enter / Meta+Enter / Alt+Enter** — insert newline (`src/components/textInput.tsx:946-948`).
- **`\` + Enter** — multi-line continuation fallback (`src/content/hotkeys.ts:34`).
- **Ctrl+C** — multi-meaning: interrupt active turn → clear composer draft → exit (`src/content/hotkeys.ts:16, src/app/useInputHandlers.ts:123-161`).
- **Cmd+C** (macOS) — copy selection (`src/content/hotkeys.ts:7, src/components/textInput.tsx:915-927`).
- **Ctrl+C** (forwarded by remote terminals) — same as Cmd+C semantics.
- **Cmd+D / Ctrl+D** — exit Hermes.
- **Cmd+G / Ctrl+G / Alt+G** — open `$EDITOR` for composer (Alt+G fallback for VSCode/Cursor).
- **Cmd+L / Ctrl+L** — `forceRedraw`.
- **Cmd+V / Alt+V / Ctrl+V / `/paste`** — paste clipboard text; `/paste` attaches clipboard image.
- **Tab** — apply current completion (`useCompletion.ts`).
- **↑/↓** in composer — cycle completions, queue edit slots, or input history depending on context.
- **Ctrl+X** — open `ActiveSessionSwitcher`; while editing a queued message, delete it (`src/content/hotkeys.ts:26`).
- **Cmd+A/E or Ctrl+A/E** — home/end of line.
- **Cmd+Z/Y or Ctrl+Z/Y** — undo/redo edits (200-entry ring).
- **Cmd+W or Ctrl+W** — delete word left.
- **Cmd+U/K or Ctrl+U/K** — delete to start/end.
- **Cmd+←/→ or Ctrl+←/→ or Alt+←/→** — word jump.
- **Home / End** — start/end of line (also kbd shortcut, not just Cmd+A/E).
- **!<cmd>** — submit as shell command.
- **{!<cmd>}** — interpolate shell output inline.

### Transcript scrolling (fall-through during prompts)

`shouldFallThroughForScroll` (`src/app/useInputHandlers.ts:38-60`) lets these always reach the global handler even while an overlay is up: wheel up/down, PageUp/PageDown, Shift+↑/↓.

### Overlay key conventions

`useOverlayKeys({ onBack, onClose })` (`src/components/overlayControls.tsx:5-19`):

- **q** — close overlay (return to composer).
- **Esc** — back one stage (e.g. provider → list) if `onBack` supplied; otherwise close.

Per-overlay specifics:

- **ApprovalPrompt** (`prompts.tsx:63-114`): ↑/↓ select 1..4 (Allow once / Allow session / Always allow / Deny), Enter confirm, 1-4 quick pick, Esc/Ctrl+C → Deny.
- **ClarifyPrompt** (`prompts.tsx:116-193`): same nav + N+1 "Other" choice toggles a free-text TextInput. Numbered 1..N quick pick.
- **ConfirmPrompt** (`prompts.tsx:195-255`): Y/N quick pick, ↑/↓ select, Enter confirm, Esc cancel.
- **SessionPicker**: ↑/↓ + Enter to resume; 1..9 quick pick; `d` then `d` to delete; Esc/q cancel (`src/components/sessionPicker.tsx:100-142`).
- **ModelPicker**: two-stage (provider → model). `d` to disconnect, `g` to toggle persist global/session, Esc to step back (`src/components/modelPicker.tsx:91-273`).
- **SkillsHub**: three stages (category → skill → actions). `i` reinspect, `x` reinstall (`src/components/skillsHub.tsx:85-179`).
- **ActiveSessionSwitcher**: Tab in "new row" opens model arg picker; Enter starts new session OR switches to selected; Ctrl+D close session; Ctrl+N new; Ctrl+R refresh.
- **AgentsOverlay**: full-screen replacement for the transcript; rotates sort + filter cycles, accordion expand-all on shift-click chevron, ←/→ walk spawn-history snapshots.

There is **no vim-style normal/insert mode** — always-insert. There are also no multi-key chords beyond modifier-arrow combos.

### Quit / interrupt / cancel

`Ctrl+C` cascade (`src/app/useInputHandlers.ts:123-161`):
1. If a clarify overlay is up → send empty answer (cancel).
2. Else if approval → respond `deny`, mark turn `outcome: 'denied'`.
3. Else if sudo → send empty password, surface "sudo cancelled".
4. Else if secret → send empty value, surface "secret entry cancelled".
5. Else if modelPicker/skillsHub/picker/agents → close that overlay.
6. Else if busy → `session.interrupt`.
7. Else if composer non-empty → clear draft.
8. Else → exit (`session.die`).

---

## 10. Banner / boot screen

### ASCII logo art — `src/banner.ts:46-53`

```
██╗  ██╗███████╗██████╗ ███╗   ███╗███████╗███████╗       █████╗  ██████╗ ███████╗███╗   ██╗████████╗
██║  ██║██╔════╝██╔══██╗████╗ ████║██╔════╝██╔════╝      ██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝
███████║█████╗  ██████╔╝██╔████╔██║█████╗  ███████╗█████╗███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║
██╔══██║██╔══╝  ██╔══██╗██║╚██╔╝██║██╔══╝  ╚════██║╚════╝██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║
██║  ██║███████╗██║  ██║██║ ╚═╝ ██║███████╗███████║      ██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║
╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝╚══════╝      ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝
```

Coloured with a six-row gradient pulling from `primary, accent, border, muted` (`src/banner.ts:73, 76-80`).

### Caduceus hero — `src/banner.ts:55-71`

15-line braille-art caduceus, shown in the `SessionPanel` left column when terminal width ≥ 90 cells (`src/components/branding.tsx:164-166, 282-303`).

### Responsive banner tiers — `src/components/branding.tsx:85-120`

- `cols < 34` — hide banner entirely.
- `cols < COMPACT_FROM (58)` — text-only fallback: `t.brand.icon t.brand.name` bold + tagline.
- `cols < logoWidth + 2 (≈98)` — compact rule version: bold-primary `── Hermes Agent ──`, centered tagline, full-width primary-coloured `─` rule.
- `cols ≥ logoWidth + 2` — full ASCII logo + `⚕ Nous Research · Messenger of the Digital Gods` tagline below.

### Welcome line

`brand.welcome` defaults to `'Type your message or /help for commands.'` (`src/theme.ts:243`). Goodbye is `'Goodbye! ⚕'`.

### SessionPanel — `src/components/branding.tsx:160-416`

Bordered (`round`) panel with caduceus hero on left (wide), model line, `Available Tools` (expanded), `Available Skills` (collapsed, N total, M categories), `System Prompt` (collapsed, char-count suffix), `MCP Servers` (collapsed, N connected). Bottom line: `{N} tools · {M} skills · {K} MCP · /help for commands`.

When `info.update_behind ≥ 1`, an attention-yellow `! N commits behind - run hermes update to update` line appears (`src/components/branding.tsx:397-411`).

### Boot connection errors

`gateway.start_timeout` event → if no recovery → status text shows `error: …`. The TUI does NOT exit on backend unavailability; it idles with `summoning hermes…` status (`src/app/uiStore.ts:27`) until the WebSocket opens or the user quits.

`session.create` failure → if backend reports "No LLM provider configured", the intro renders the SETUP_REQUIRED panel with `/model` / `/setup` / Ctrl+C exit options (`src/app/createGatewayEventHandler.ts:24, src/content/setup.ts:5-17`).

---

## 11. Gateway client — `src/gatewayClient.ts` (730 LOC)

### Lifecycle

`new GatewayClient()` (`src/gatewayClient.ts:132-156`). `gw.start()` either:
- spawns a Python child process and opens a websocket against its STDIO+TCP, OR
- attaches to a pre-existing gateway URL (`HERMES_TUI_GATEWAY_URL` env), OR
- attaches both an attach-mode socket + a sidecar (`HERMES_TUI_SIDECAR_URL`).

### Endpoint catalog (high-traffic ones)

(Resolved by grepping every `rpc<X>('endpoint', …)` call across the slash-command files + state hooks.)

- `command.dispatch` — server-dispatched slash; returns one of `{type:'exec'|'plugin'|'alias'|'skill'|'send'}` (`gatewayTypes.ts:46-50`).
- `complete.slash`, `complete.path` — autocomplete.
- `config.get` / `config.set` / `config.full` / `config.mtime` — config CRUD; full poll periodically.
- `session.create`, `session.resume`, `session.activate`, `session.list`, `session.delete`, `session.most_recent`, `session.title`, `session.save`, `session.undo`, `session.status`, `session.usage`, `session.compress`, `session.branch`, `session.close`, `session.interrupt`, `session.steer`.
- `session.active.list` — TUI orchestrator surface.
- `prompt.submit`, `prompt.background`.
- `clarify.respond`, `approval.respond`, `sudo.respond`, `secret.respond`.
- `shell.exec`, `clipboard.paste`, `input.detect_drop`, `terminal.resize`, `image.attach`.
- `voice.toggle`, `voice.record`.
- `tools.configure`, `model.options`, `model.save_key`, `model.disconnect`.
- `reload.mcp`, `reload.env`, `process.stop`, `browser.manage`.
- `rollback.list`, `rollback.diff`, `rollback.restore`.
- `skills.manage` (action `list|inspect|search|install|browse|reload`).
- `delegation.status`, `delegation.pause`, `subagent.interrupt`.
- `spawn_tree.list`, `spawn_tree.load`, `spawn_tree.save`.

### Event protocol — `GatewayEvent` union (`src/gatewayTypes.ts:497-562`)

44 event types. Key categories:
- `gateway.ready`, `gateway.start_timeout`, `gateway.stderr`, `gateway.protocol_error`.
- `skin.changed`, `session.info`, `status.update`.
- `thinking.delta`, `reasoning.delta`, `reasoning.available`.
- `message.start`, `message.delta`, `message.complete`.
- `tool.generating`, `tool.start`, `tool.progress`, `tool.complete`.
- `clarify.request`, `approval.request`, `sudo.request`, `secret.request`.
- `subagent.spawn_requested`, `subagent.start`, `subagent.thinking`, `subagent.tool`, `subagent.progress`, `subagent.complete`.
- `voice.status`, `voice.transcript`, `browser.progress`, `background.complete`, `review.summary`, `error`.

### Buffered events

Up to 2000 events are buffered while no React subscriber is attached (`src/gatewayClient.ts:142, 157-172`). Once `gw.on('event', …)` fires, buffered events flush; from then on it's streaming.

### Timeouts + redaction

- RPC default 120s timeout (env override `HERMES_TUI_RPC_TIMEOUT_MS`).
- Startup default 15s (env override `HERMES_TUI_STARTUP_TIMEOUT_MS`, floor 5s).
- All URLs scrubbed before logging (`src/gatewayClient.ts:102-122`).

### Reconnect / session restoration

Reconnect handled by the gateway child's lifecycle and the buffered-event replay. On `gateway.ready`, the handler picks up `STARTUP_RESUME_ID` env (`src/config/env.ts:27`) and either resumes that id or starts fresh. `session.activate` returns a `running` flag + `inflight` turn so the TUI can resume mid-stream (`src/gatewayTypes.ts:150-160`).

---

## 12. What we'd port vs drop for v4

### Port verbatim — visual patterns clearly load-bearing

1. **Per-turn separator `───`** in `border` color between user turns. Cheap, makes long transcripts scannable (`src/components/appLayout.tsx:107-111`).
2. **Response separator (`└─ Response`)** between meta block and assistant text — disambiguates "what the agent is thinking" from "what the agent is saying" (`src/components/messageLine.tsx:200-209`).
3. **Tree-stem ToolTrail** — `├─`/`└─` accordion with chevron toggles, heat-graded stem color for hot branches, per-section counts and token-suffix. This is the single best information density pattern in the UI (`src/components/thinking.tsx:60-125, 280-617`).
4. **Live cursor `▍`** at the end of streaming text. Reads as "the agent is still typing" without burning a separate progress widget (`src/components/thinking.tsx:194-230`).
5. **FaceTicker** — rotating kaomoji + verb-of-the-moment + elapsed in the status bar while busy (`src/components/appChrome.tsx:78-124`). The verbs list (`pondering`, `contemplating`, etc.) gives personality without cliché.
6. **Context-percent bar** with color escalation (green 0–50, gold 50–80, orange 80–95, red 95+) in `StatusRule` (`src/components/appChrome.tsx:126-144`).
7. **StickyPromptTracker** — when the latest user message scrolls off-screen, a `↳ <truncated user prompt>` line above the composer reminds the user what they asked (`src/components/appChrome.tsx:442-449`, `src/domain/viewport.ts:18-51`). Excellent affordance for long replies.
8. **Live `TodoPanel` anchored to the latest user msg row** so the live plan visually belongs to the prompt (`src/components/appLayout.tsx:67-77, 133`).
9. **Queued-messages window** above the composer with `▸ editing N` + Ctrl+X delete (`src/components/queuedMessages.tsx:17-57`).
10. **Slash autocomplete dropdown** appearing above the composer, with `display | meta` two-column rows and fixed 16-row viewport centred on the highlight (`src/components/appOverlays.tsx:215-251`).
11. **Approval prompt visual design**: double-bordered warn-colored box with `⚠ approval required · {description}`, 10-line command preview, `▸` glyph + inverse-highlight selection (`src/components/prompts.tsx:80-113`).
12. **SessionPanel collapsible accordion** for tools/skills/system-prompt/MCP — surfaces the heaviest context without dominating the screen (`src/components/branding.tsx:160-416`).
13. **Markdown table tier-3 wrap** — proportional shrink + vertical fallback when 2-3 col tables would overflow as wrapped rows (`src/components/markdown.tsx:286-477`). Markdown tables almost always render acceptably.
14. **`!cmd` shell-mode prompt color swap** to `shellDollar` blue and `$` glyph (`src/components/appLayout.tsx:290-296`). Immediate visual confirmation that you're about to shell out.
15. **Cmp-N compression count** + warn/error escalation in the status bar (`src/components/appChrome.tsx:375-384`). Subtle, but the only way to see context-pressure history.

### Port with adaptation — patterns where v4's stack needs different mechanics

1. **Streaming-markdown prefix-caching** (`src/components/streamingMarkdown.tsx`) — the *invariant* is right (split at last stable block boundary, memoize prefix). The *mechanism* depends on having React.memo and a stable text-keyed cache. In v4 web-dashboard, the same shape works with React.memo or with SolidJS-style memos. In a Rust TUI (ratatui), reuse buffered-frame caching at the line level.
2. **Tree-stem accordion** — the rail-string pattern (`│ │ ├─ `) is universal but the React layout uses Yoga flex. In ratatui, a simpler approach is per-level indent + computed prefix string. The KEY adaptation: the *heat-map gradient* (cold→warm stem color when a branch is "hot") (`src/components/thinking.tsx:267-279`). It's worth keeping the gradient even if the rails are simpler.
3. **Slash-command UI**: Hermes parses server-side + supplements with local UI. v4 has its own slash registry; preserve the `Panel` rendering (`src/components/branding.tsx:418-453`) for `/help` output so the user sees grouped categories with two-column rows.
4. **Indicator-style choice**: keep `kaomoji` (default), `unicode`, `ascii`; v4's "compact mode" might want `unicode` as default since it's smaller (`src/components/appChrome.tsx:47-76`).
5. **Mouse selection model** — fundamental to Hermes feel. Web dashboard inherits this from the browser; Rust TUI needs to port the selection state machine (`packages/hermes-ink/src/ink/selection.ts:1-1143`, 1143 LOC). That's a huge surface but the principles are clear.
6. **Approval / clarify / confirm / sudo / secret prompts** — port the visual design (double border, warn color, `▸` glyph, quick-pick numbers, Y/N for confirm). For web dashboard, render as modal sheets that still emphasise the same prefix glyphs.
7. **Long-system-message collapse** (system prompt, AGENTS.md) — `▸ first-line — N chars` chevron (`src/components/messageLine.tsx:122-138`). Critical UX for `/status` and intro panels with full system prompts.
8. **StatusRule** — port the segment-with-` │ ` separators pattern, but: on web dashboard those should be real CSS borders, not Unicode bars. The information ordering is right: status text → model → tokens → context% → uptime → cmp → spawn HUD → voice → sessions → bg → cost.
9. **SkillsHub + ModelPicker overlays** — port the three-stage navigation pattern (category → item → actions). For web dashboard, that's three left-rail panes. For ratatui, three stacked sub-modes.
10. **GoodVibesHeart easter egg** — port verbatim; it's tiny and humanizes the agent. Keep the trigger regex and color flash.

### Drop — things specific to Hermes's workflow that don't apply

1. **`useDeclaredCursor` + `useCursorAdvance` hardware-cursor sync** — only relevant in terminals. Browser cursors are positioned by the contenteditable / textarea natively.
2. **AlternateScreen + bracket-paste setup** — terminal-only chrome.
3. **Mouse-tracking modes** (`off/wheel/buttons/all`) — terminal-specific DEC sequences (`packages/hermes-ink/src/ink/termio/dec.ts`).
4. **OSC52 clipboard fallback** — only relevant when the host terminal can't reach pbcopy/xclip (`src/lib/osc52.ts`). Web has a real clipboard API.
5. **Termux-mode special cases** — single-cell ASCII prompts, inline-mode default, fast-echo opt-out (`src/lib/termux.ts`, `src/config/env.ts:25, 60`). Mobile considerations don't translate.
6. **Heap dump / mem panel `/heapdump` `/mem`** — Node-specific diagnostics.
7. **Voice integration** (`voice.toggle`, voice record key, Whisper STT plumbing) — pure backend concern; mark as future work.
8. **FPS overlay** (`HERMES_TUI_FPS=1`) — terminal-rendering specific; replace with React DevTools-style profiler in v4.
9. **CompactBanner / responsive cols-based banner tiers** — web dashboard has CSS responsive design; v4 picks one banner.
10. **Ink reconciler internals** (`packages/hermes-ink/src/ink/reconciler.ts`, `screen.ts`, `dom.ts`, `renderer.ts`, `output.ts`, `render-node-to-output.ts` — combined ~5,500 LOC) — entire layer goes away.

---

## 13. Open questions

1. **Mouse tracking precedence** — `HERMES_TUI_MOUSE_TRACKING` vs `HERMES_TUI_DISABLE_MOUSE` vs Termux default: env.ts:30-48 documents the order but the actual `/mouse` runtime command can put state out of sync with config until next mtime poll (`src/app/slash/commands/core.ts:131-147`). Spec'd as "intentional" but I couldn't confirm.
2. **Sticky bottom scroll behavior** — `ScrollBox stickyScroll` keeps the viewport pinned to the bottom while new content arrives. The exact trigger for "user scrolled up → stop sticking" lives in `packages/hermes-ink/src/ink/components/ScrollBox.tsx` (290 LOC), which I did not fully read in this pass.
3. **`turnController` lifecycle** — file is 866 lines (`src/app/turnController.ts`); I have its inferred shape from `createGatewayEventHandler.ts` interactions but did not read it line-by-line. Key open questions: how exactly does it batch reasoning vs. tool deltas, and what is `boostStreamingForScroll`/`relaxStreaming` doing (`src/app/useInputHandlers.ts:101-107`)?
4. **`useMainApp` orchestration** — 1,039 lines; I read the first 200 only. The exact wiring of `slashRef`, `submitRef`, `onEventRef`, and how those connect the gateway events to the various sub-hooks (composer state, input handlers, session lifecycle, submission) needs a second pass.
5. **`useVirtualHistory`** — 554 lines (`src/hooks/useVirtualHistory.ts`); not read. This is the virtualisation system for the transcript and is non-trivial.
6. **`createSlashHandler.ts`** (130 LOC) and `createGatewayEventHandler.ts` (751 LOC) — I read the first 200 lines of the event handler; the bulk handling of `tool.start`, `tool.complete`, `subagent.*`, `message.delta` deserves a re-read to confirm exact ordering and edge cases.
7. **`AgentsOverlay` full UX** (1073 LOC) — I read the first 200 lines covering the data model and scrollbar; the sort/filter/expand-all UX, history walking, and pause/interrupt buttons were not fully traced.
8. **`activeSessionSwitcher.tsx` full UX** (635 LOC) — first 150 lines read; the "new row" prompt input + model-arg selection flow needs a deeper pass.
9. **`useConfigSync.ts`** (288 lines) — periodic poll of `config.mtime` + reload of `config.full`. Not read; need to confirm cadence (default 5s? exposed in `src/config/timing.ts`?).
10. **What does the gateway's `command.dispatch` actually do for slash commands that are NOT in `SLASH_COMMANDS`?** Local registry has ~50 commands; the catalog reports far more (`skill_count` field). Backend handles those itself by routing through `command.dispatch` and returning typed responses (`src/gatewayTypes.ts:46-50`). The TUI's local handlers always preempt; backend ones run after.

---

## Appendix A: file-by-file summary

### Root entry & app

- **`src/entry.tsx`** (104 LOC) — Node bootstrap. Forces truecolor before any chalk import. Sets up graceful exit, memory monitor, optional heap-dump-on-start. Resets terminal modes before mounting Ink. Dynamic-import of app code lazy-loads `@hermes/ink`, `./app`, `./lib/perfPane`, `./lib/fpsStore`. Renders `<App gw={gw}>` with `exitOnCtrlC: false` and a hyperlink-click handler that opens URLs via `openExternalUrl`.
- **`src/app.tsx`** (25 LOC) — Wraps `<AppLayout>` in a `<GatewayProvider>`. Reads `mouseTracking` from `$uiState`. Hook entry-point is `useMainApp(gw)`.
- **`src/app/useMainApp.ts`** (1039 LOC) — Master orchestrator. Wires the gateway event handler, slash handler, composer state, input handlers, submission, session lifecycle, config sync, long-run tool charms. Exports `startPromptLiveSession()` helper for the orchestrator "new prompt" row. Defines `MAX_HEIGHT_CACHE_BUCKETS`, `GOOD_VIBES_RE`, `BRACKET_PASTE_ON/OFF`. Manages `historyItems`, `lastUserMsg`, `stickyPrompt`, `catalog`, voice flags, session-started-at, turn-started-at, goodVibesTick, bellOnComplete.
- **`src/app/turnController.ts`** (866 LOC) — Singleton controller for streaming deltas → `$turnState`. `boostStreamingForScroll`/`relaxStreaming` adjust batching during scroll. `recordMessageComplete`, `persistSpawnTree`, `clearStatusTimer`. Not fully read.

### Theme + branding

- **`src/theme.ts`** (589 LOC) — `DARK_THEME`, `LIGHT_THEME`, `DEFAULT_THEME`. Light-mode detection via 5 layered signals. ANSI normalisation for non-truecolor Apple Terminal. `fromSkin()` merges backend-provided skin overrides.
- **`src/banner.ts`** (93 LOC) — ASCII logo (6 rows), caduceus art (15 rows), rich-markup parser for custom skin banners.
- **`src/components/branding.tsx`** (466 LOC) — `<Banner>` (responsive 4-tier), `<SessionPanel>` (intro panel with accordion sections), `<Panel>` (generic titled panel for slash output).

### Layout + chrome

- **`src/components/appLayout.tsx`** (444 LOC) — `<AppLayout>` root. Decomposes into `<TranscriptPane>`, `<ComposerPane>`, `<PromptZone>`, `<FloatingOverlays>`, `<AgentsOverlayPane>`, `<StatusRulePane>`. Handles `INLINE_MODE` (skip AlternateScreen) and `SHOW_FPS`.
- **`src/components/appChrome.tsx`** (554 LOC) — `<StatusRule>`, `<TranscriptScrollbar>`, `<StickyPromptTracker>`, `<FloatBox>`, `<FaceTicker>`, `<SpawnHud>`, `<GoodVibesHeart>`. Status bar maths and indicator-style rendering.
- **`src/components/appOverlays.tsx`** (254 LOC) — `<PromptZone>` mounts approval/clarify/confirm/sudo/secret in the prompt area; `<FloatingOverlays>` floats picker/modelPicker/sessions/skillsHub/pager + autocomplete dropdown above the composer.

### Message rendering

- **`src/components/messageLine.tsx`** (237 LOC) — Per-message dispatcher. Routes to `<TodoPanel>`, `<ToolTrail>`, `<Md>`, `<StreamingMd>`, or plain `<Text>` based on `role` + `kind`. Glyph + body coloured per `ROLE`.
- **`src/components/streamingAssistant.tsx`** (110 LOC) — Live streaming area. Renders pending segments, active-tools shelf, in-flight message. `<LiveTodoPanel>` for live todos.
- **`src/components/markdown.tsx`** (1119 LOC) — Full Markdown subset renderer. Inline regex catalog, block dispatcher, table layout (3-tier), code blocks with syntax highlighting, diff blocks, math rendering. Theme-keyed LRU cache.
- **`src/components/streamingMarkdown.tsx`** (174 LOC) — Prefix-caching incremental markdown. Boundary detection skips fenced + math blocks.
- **`src/components/thinking.tsx`** (1215 LOC) — `<Thinking>`, `<ToolTrail>`, `<SubagentAccordion>`, `<Spinner>`, `<StreamCursor>`, `<Chevron>`, `<TreeRow>`, `<TreeNode>`. Tree-rail rendering, expand-all on shift-click, heat gradient on stems, per-tool elapsed.
- **`src/components/todoPanel.tsx`** (93 LOC) — Live and archived todo list with chevron, done/total counter, incomplete-pending suffix.

### Composer

- **`src/components/textInput.tsx`** (1312 LOC) — Full grapheme-aware text editor. Mouse drag selection, right-click copy/paste, fast-echo bypass for ASCII appends/backspaces, undo/redo ring, bracket-paste handling, line-nav, word-jump, voice-pass-through, IME-safe.
- **`src/components/queuedMessages.tsx`** (65 LOC) — Sliding window over queued messages, `▸ editing N` cursor, Ctrl+X delete hint.
- **`src/components/helpHint.tsx`** (73 LOC) — Floating help panel triggered by typing `?` only.
- **`src/components/maskedPrompt.tsx`** (34 LOC) — Sudo/secret password input with `*` mask.
- **`src/components/prompts.tsx`** (276 LOC) — `<ApprovalPrompt>`, `<ClarifyPrompt>`, `<ConfirmPrompt>`. Pure `approvalAction(ch, key, sel)` reducer exported for testing.

### Overlays

- **`src/components/sessionPicker.tsx`** (227 LOC) — Resume-previous-session list with `d d` delete confirmation.
- **`src/components/modelPicker.tsx`** (527 LOC) — Two-stage provider → model picker. `g` toggle session/global persist. `d` disconnect provider. Inline API-key entry.
- **`src/components/activeSessionSwitcher.tsx`** (635 LOC) — Orchestrator overlay for live TUI sessions: switch/close/new-with-prompt-and-model.
- **`src/components/agentsOverlay.tsx`** (1073 LOC) — Full-screen spawn-tree dashboard. Sort modes (depth/tools/duration/status), filter modes (all/running/failed/leaf), per-node expand, pause/interrupt, history walking.
- **`src/components/skillsHub.tsx`** (308 LOC) — Three-stage category → skill → actions UI. Inspect + reinstall.
- **`src/components/overlayControls.tsx`** (50 LOC) — `<OverlayHint>` + `useOverlayKeys` + `windowItems/windowOffset` helpers.

### State + hooks

- **`src/app/uiStore.ts`** (44 LOC) — `$uiState` atom.
- **`src/app/turnStore.ts`** (85 LOC) — `$turnState` atom with `useTurnSelector`, `archiveTodosAtTurnEnd`, `toggleTodoCollapsed`.
- **`src/app/overlayStore.ts`** (53 LOC) — `$overlayState` + `$isBlocked` computed + `resetFlowOverlays` soft reset.
- **`src/app/delegationStore.ts`** (77 LOC) — `$delegationState` + `$overlaySectionsOpen`.
- **`src/app/spawnHistoryStore.ts`** (159 LOC) — Persisted spawn-tree snapshots + diff state.
- **`src/app/inputSelectionStore.ts`** (15 LOC) — Cross-component handle to the composer's selection state.
- **`src/app/scroll.ts`** (71 LOC) — `scrollWithSelectionBy` keeps text selection in sync with transcript scroll.
- **`src/hooks/useVirtualHistory.ts`** (554 LOC) — Transcript virtualisation. Not read in this pass.
- **`src/hooks/useCompletion.ts`** (112 LOC) — Tab-completion via `complete.slash`/`complete.path` RPC.
- **`src/hooks/useInputHistory.ts`** (11 LOC) — In-memory ring of submitted prompts.
- **`src/hooks/useQueue.ts`** (76 LOC) — Composer queue ref + actions.
- **`src/hooks/useGitBranch.ts`** (72 LOC) — Async git branch detection for cwd label.

### App glue

- **`src/app/useComposerState.ts`** (367 LOC) — Composer state machine: input, inputBuf, completions, history index, queue edit index, paste snippets.
- **`src/app/useInputHandlers.ts`** (576 LOC) — Global keyboard router. Ctrl+C cascade, wheel accel, scroll-with-selection, voice keypress routing, queue cycle.
- **`src/app/useSubmission.ts`** (429 LOC) — Submit pipeline: slash dispatch, queued-message handling, busy-mode behavior.
- **`src/app/useSessionLifecycle.ts`** (366 LOC) — New/resume/activate/close. Sets up startup resume from `HERMES_TUI_RESUME`.
- **`src/app/useConfigSync.ts`** (288 LOC) — Polls `config.mtime`; reloads `config.full` on change.
- **`src/app/useLongRunToolCharms.ts`** (69 LOC) — Periodically updates status with "still cooking…" charms.
- **`src/app/createGatewayEventHandler.ts`** (751 LOC) — Maps every `GatewayEvent` to UI state. Read first 200 LOC only.
- **`src/app/createSlashHandler.ts`** (130 LOC) — Slash dispatcher: local registry → `command.dispatch` fallback.
- **`src/app/setupHandoff.ts`** (54 LOC) — Suspend Ink, exec `hermes setup`, resume.

### Slash commands

- **`src/app/slash/registry.ts`** (20 LOC) — Aggregates all slash commands.
- **`src/app/slash/types.ts`** (21 LOC) — `SlashCommand` interface.
- **`src/app/slash/commands/core.ts`** (648 LOC) — 22 core commands.
- **`src/app/slash/commands/session.ts`** (554 LOC) — 14 session commands.
- **`src/app/slash/commands/ops.ts`** (717 LOC) — Operational commands.
- **`src/app/slash/commands/setup.ts`** (20 LOC) — `/setup`.
- **`src/app/slash/commands/debug.ts`** (48 LOC) — `/heapdump`, `/mem`.

### Domain

- **`src/domain/messages.ts`** (91 LOC) — `introMsg`, `userDisplay`, `attachedImageNotice`, `toTranscriptMessages`, `fmtDuration`.
- **`src/domain/roles.ts`** (9 LOC) — `ROLE` table.
- **`src/domain/details.ts`** (76 LOC) — `parseDetailsMode`, `sectionMode`, `nextDetailsMode`, `resolveDetailsMode`, `SECTION_DEFAULTS`.
- **`src/domain/slash.ts`** (10 LOC) — `looksLikeSlashCommand`, `parseSlashCommand`, `TUI_SESSION_MODEL_FLAG`.
- **`src/domain/viewport.ts`** (51 LOC) — `stickyPromptFromViewport` computes the recap line based on what's visible.
- **`src/domain/providers.ts`** (11 LOC) — Disambiguates provider display names by slug when duplicated.
- **`src/domain/paths.ts`** (16 LOC) — `shortCwd`, `fmtCwdBranch` for status bar right-side.
- **`src/domain/usage.ts`** (3 LOC) — `ZERO` constant for `Usage`.

### Config + content

- **`src/config/env.ts`** (64 LOC) — All env-var flags: `HERMES_TUI_RESUME`, `HERMES_TUI_QUERY`, `HERMES_TUI_IMAGE`, `HERMES_TUI_MOUSE_TRACKING`, `HERMES_TUI_INLINE`, `HERMES_TUI_FPS`, `HERMES_TUI_NO_CONFIRM`, etc. Termux mode detection.
- **`src/config/limits.ts`** (13 LOC) — `LARGE_PASTE`, `LIVE_RENDER_MAX_CHARS=16000`, `LIVE_RENDER_MAX_LINES=240`, `LONG_MSG=300`, `MAX_HISTORY=800`, `THINKING_COT_MAX=160`, `WHEEL_SCROLL_STEP=1`.
- **`src/config/timing.ts`** (6 LOC) — `STREAM_BATCH_MS=16`, `STREAM_IDLE_BATCH_MS=16`, `STREAM_SCROLL_BATCH_MS=96`, `STREAM_TYPING_BATCH_MS=80`, `TYPING_IDLE_MS=250`, `REASONING_PULSE_MS=700`.
- **`src/content/faces.ts`** (17 LOC) — 15 kaomoji.
- **`src/content/verbs.ts`** (38 LOC) — 15 thinking verbs + tool-verb map.
- **`src/content/hotkeys.ts`** (37 LOC) — Hotkey table for `/help` and `<HelpHint>`.
- **`src/content/fortunes.ts`** (30 LOC) — 10 normal fortunes + 3 legendary drops (1-in-20 rare).
- **`src/content/placeholders.ts`** (13 LOC) — 7 random composer placeholders.
- **`src/content/charms.ts`** (1 LOC) — 3 long-run-tool charm strings.
- **`src/content/setup.ts`** (17 LOC) — `buildSetupRequiredSections()` for the missing-provider panel.

### Lib

- **`src/lib/text.ts`** (339 LOC) — `compactPreview`, `boundedLiveRenderText`, `formatToolCall`, `parseToolTrailResultLine`, `splitToolDuration`, `toolTrailLabel`, `hasAnsi`, `sanitizeAnsiForRender`, `stripAnsi`, `flat`, `fmtK`, `estimateTokensRough`, `thinkingPreview`, `pick`.
- **`src/lib/syntax.ts`** (117 LOC) — Tiny syntax highlighter for 8 languages.
- **`src/lib/subagentTree.ts`** (355 LOC) — `buildSubagentTree`, `treeTotals`, `widthByDepth`, `sparkline`, `peakHotness`, `hotnessBucket`, `formatSummary`, `flattenTree`, `descendantIds`, `topLevelSubagents`.
- **`src/lib/mathUnicode.ts`** (770 LOC) — LaTeX → Unicode translator. Sentinel-marked `\boxed{}` regions.
- **`src/lib/inputMetrics.ts`** (203 LOC) — `cursorLayout`, `offsetFromPosition`, `transcriptBodyWidth`, `transcriptGutterWidth`, `composerPromptWidth`, `stableComposerColumns`, `inputVisualHeight`.
- **`src/lib/prompt.ts`** (35 LOC) — Composer prompt-text resolution (shell-mode, termux-mode, profile-mode).
- **`src/lib/platform.ts`** (409 LOC) — `isMac`, `isRemoteShell`, `isActionMod`, `isMacActionFallback`, `isVoiceToggleKey`, `parseVoiceRecordKey`, `formatVoiceRecordKey`, `DEFAULT_VOICE_RECORD_KEY` (`Ctrl+B`).
- **`src/lib/messages.ts`** (8 LOC) — `appendTranscriptMessage` (capped insert).
- **`src/lib/clipboard.ts`** (166 LOC) — `readClipboardText`, `writeClipboardText` with pbcopy/wl-copy/xclip fallback.
- **`src/lib/osc52.ts`** (73 LOC) — `writeOsc52Clipboard` ANSI escape clipboard fallback.
- **`src/lib/externalLink.ts`** (435 LOC) — URL title fetcher with caching.
- **`src/lib/externalCli.ts`** (16 LOC) — `launchHermesCommand` for `/setup`.
- **`src/lib/forceTruecolor.ts`** (60 LOC) — Pre-boot chalk forcing.
- **`src/lib/perfPane.tsx`** (107 LOC) — Debug perf timer wrapping a child component.
- **`src/lib/fpsStore.ts`** (51 LOC) — `$fpsState` atom + `trackFrame`.
- **`src/lib/memoryMonitor.ts`** (109 LOC) — Periodic heap watch with `onHigh`/`onCritical` callbacks.
- **`src/lib/memory.ts`** (187 LOC) — `formatBytes`, `performHeapDump`, `HeapDumpResult`.
- **`src/lib/gracefulExit.ts`** (47 LOC) — `setupGracefulExit` registers signal handlers.
- **`src/lib/terminalModes.ts`** (51 LOC) — DEC mode reset for boot/exit safety.
- **`src/lib/terminalSetup.ts`** (444 LOC) — IDE keybinding configuration writers for VSCode/Cursor/Windsurf.
- **`src/lib/terminalParity.ts`** (78 LOC) — Detects deficient terminals and surfaces hints.
- **`src/lib/openExternalUrl.ts`** (158 LOC) — Cross-platform URL open (uses tmux passthrough, OSC 8, `open`/`xdg-open`/`start`).
- **`src/lib/precisionWheel.ts`** (48 LOC) — Modifier-held wheel for 1-line-at-a-time scrolling.
- **`src/lib/wheelAccel.ts`** (190 LOC) — Wheel accel ramp on sustained scroll.
- **`src/lib/virtualHeights.ts`** (145 LOC) — Estimated message height for virtualisation.
- **`src/lib/viewportStore.ts`** (124 LOC) — `useViewportSnapshot`, `useScrollbarSnapshot`.
- **`src/lib/history.ts`** (82 LOC) — Persistence helper.
- **`src/lib/circularBuffer.ts`** (48 LOC) — Capped append-only ring.
- **`src/lib/rpc.ts`** (41 LOC) — `asRpcResult`, `rpcErrorMessage`, `RpcResult` type alias.
- **`src/lib/liveProgress.ts`** (79 LOC) — `appendToolShelfMessage`, `countPendingTodos`, `isTodoDone`.
- **`src/lib/todo.ts`** (9 LOC) — `todoGlyph`, `todoTone`.
- **`src/lib/emoji.ts`** (55 LOC) — `ensureEmojiPresentation` for proper width.
- **`src/lib/reasoning.ts`** (55 LOC) — Reasoning utilities.
- **`src/lib/editor.ts`** (47 LOC) — `$EDITOR` invocation for composer.
- **`src/lib/termux.ts`** (29 LOC) — Termux detection.

### @hermes/ink (vendored fork)

Key components (omitting tests):
- **`packages/hermes-ink/src/ink/ink.tsx`** (2705 LOC) — Reconciler entry + renderer orchestration.
- **`packages/hermes-ink/src/ink/screen.ts`** (1590 LOC) — Screen diff + cell-level update.
- **`packages/hermes-ink/src/ink/render-node-to-output.ts`** (1582 LOC) — DOM node → output bytes.
- **`packages/hermes-ink/src/ink/selection.ts`** (1143 LOC) — Mouse selection state machine.
- **`packages/hermes-ink/src/ink/parse-keypress.ts`** (924 LOC) — Keypress parser (Ctrl/Alt/Meta/Cmd resolution, ANSI escape sequences).
- **`packages/hermes-ink/src/ink/output.ts`** (845 LOC) — Output buffer + stream write.
- **`packages/hermes-ink/src/ink/components/App.tsx`** (822 LOC) — Top-level Ink app component with input dispatching, terminal-focus tracking, error boundary.
- **`packages/hermes-ink/src/ink/log-update.ts`** (752 LOC) — Differential log-update writer.
- **`packages/hermes-ink/src/ink/termio/osc.ts`** (724 LOC) — OSC sequence handler (titles, hyperlinks, palette queries).
- **`packages/hermes-ink/src/ink/styles.ts`** (750 LOC) — Yoga + chalk style mappings.
- **`packages/hermes-ink/src/ink/termio/parser.ts`** (467 LOC) — ANSI parser.
- **`packages/hermes-ink/src/ink/dom.ts`** (495 LOC) — Virtual DOM nodes.
- **`packages/hermes-ink/src/ink/components/Box.tsx`** (294 LOC) — `<Box>` with flex layout, borders, padding, mouse handlers.
- **`packages/hermes-ink/src/ink/components/Text.tsx`** (336 LOC) — `<Text>` with color/wrap/dim/bold/italic/strikethrough/underline/inverse.
- **`packages/hermes-ink/src/ink/components/ScrollBox.tsx`** (290 LOC) — `<ScrollBox>` with sticky-bottom, viewport accounting, scrollbar handle.
- **`packages/hermes-ink/src/ink/components/Button.tsx`** (236 LOC) — Mouse-click button primitive.
- **`packages/hermes-ink/src/ink/Ansi.tsx`** (435 LOC) — Pre-tokenised ANSI passthrough renderer.
- **`packages/hermes-ink/src/native-ts/yoga-layout/index.ts`** (2326 LOC) — Hand-ported subset of Yoga flexbox.
- **`packages/hermes-ink/index.d.ts`** (176 LOC) — Public API surface declaration.

---

*End of spec.*
