# Hermes TUI — patterns we kept, patterns we dropped

Date: 2026-05-27
Reader: anyone porting more of Hermes's chat UX into `evy-chat-tui`.

These notes capture what the `hermes-chat-tui-builder` worker found
while reading `/Users/sem/code/hermes-agent` for Phase 6. We did NOT
port verbatim — Hermes is Python + `prompt_toolkit`, evy-chat-tui is
Rust + `ratatui`. The line citations below are anchors so the next
porter can verify each claim against the source.

The brief mentioned `prompt_toolkit==3.0.52` from `pyproject.toml`.
That IS Hermes's chat-input library. The fancier full-screen TUI under
`hermes-agent/ui-tui/` is actually TypeScript + Ink (React for the
terminal). For Phase 6 the relevant code is the Python REPL in
`cli.py` (~15K lines) — that's what the operator interacts with on
`hermes chat`.

## Patterns we ported

### 1. Slash-command discrimination from pasted paths

**Source:** `hermes-agent/cli.py:2735-2750` — `_looks_like_slash_command`.

Hermes distinguishes `/help` from `/Users/foo/bar.md` by checking
whether the first whitespace-delimited word contains any `/` after the
leading one. A path always has at least one more.

We ported the **rule** (a slash command is `/word` with no internal
slashes), not the helper. Evy's slash parser in `input.rs::SlashCommand::parse`
splits on whitespace and switches on the first token verbatim — same
practical effect for our short, controlled command set
(`/quit`, `/help`, `/clear`, `/new-session`, `/exit`, `/q`, `/?`, `/h`).

### 2. Shift-Enter / Ctrl-J as newline aliases

**Source:** `hermes-agent/hermes_cli/pt_input_extras.py:14-52`.

Hermes ships a tiny adapter that maps Kitty / xterm modify-other-keys
sequences for Shift+Enter to the same `(Escape, ControlM)` tuple that
Alt+Enter produces, because terminals differ on which sequence Enter
emits when modified.

Our crossterm-based handler reads key events post-decode, so we don't
need the byte-level mapping. We DID port the **intent**: in
`input.rs::handle_key`, Alt+Enter inserts a newline, and Ctrl+J is
mapped to the same effect as a terminal fallback (some terminals
intercept Alt). Hermes's Hermes-specific Shift+Enter handling is left
as a TODO comment — we don't have a configuration story yet for
modify-other-keys terminals over SSH.

### 3. Two-pane layout (scrollback + input box)

**Source:** `hermes-agent/cli.py:14638-14653` — `app.run()` inside
`patch_stdout()` against an `HSplit` layout.

Hermes uses `prompt_toolkit.layout.HSplit` with a stream view on top
and a `TextArea` on the bottom. We mirror this exactly in
`ui.rs::render` using `ratatui::layout::Layout` vertical with
`Constraint::Min(5)` for scrollback and `Constraint::Length(6)` for
the input box. A 1-line footer carries the connection status — Hermes
uses prompt_toolkit's `ConditionalContainer` for similar status flags;
ours is a plain bordered paragraph because we don't have transient
status modes yet.

### 4. Slash-command exit semantics (`/quit` like Ctrl-C)

**Source:** `hermes-agent/cli.py:6517, 6638` — slash-handler returning
False is honored "like /quit".

We make `/quit`, `/q`, `/exit`, Esc, and Ctrl-C all produce
`KeyOutcome::Quit` and set `app.should_quit`. The run loop checks the
flag after the next paint so the operator sees their last input one
last time before the screen clears.

### 5. /clear-then-confirm convention deferred

**Source:** `hermes-agent/cli.py:7298` — `/clear` opens a confirmation
panel before dropping the session.

We dropped the confirmation. Phase 6 is a thin chat surface; if the
operator types `/clear` they meant it. If the workflow demands undo
later, we'd add a single-step confirmation as a status banner ("press
Enter to confirm, any other key to cancel") rather than a modal
because modal layers are awkward in ratatui.

## Patterns we deliberately did NOT port

### A. Streaming token rendering

Hermes streams LLM output via prompt_toolkit's `patch_stdout` context
and a background queue — see `cli.py:5002` ("sequences aren't garbled
by patch_stdout's StdoutProxy") and the surrounding region where
`asyncio.run_coroutine_threadsafe` posts tokens.

evy-thinking's `LlmBackend::respond` is one-shot. Streaming would need
a sibling `stream_response` method on the trait plus an SSE flavor on
the chat endpoint. We picked non-streaming for Phase 6 to ship within
budget. **Phase 7 input:** a streaming variant on the trait is the
single biggest change; the TUI's run loop is already structured around
`tokio::select!` against a result channel — swapping the result
channel for a token channel is a one-evening refactor.

### B. FileHistory + completion menus

**Source:** `hermes-agent/cli.py:57` (`from prompt_toolkit.history import FileHistory`)
and `cli.py:65` (`CompletionsMenu`).

Hermes persists input history to disk and shows a completion menu.
Both are great for power-users but not free in ratatui — completion
needs widget composition we don't have. **Phase 7 input:** if we add
history, the persistence path is `~/.evy/chat-history.toml`. Don't
reach for serde_json — TOML keeps the file human-editable.

### C. Bracketed-paste recovery

**Source:** `hermes-agent/cli.py:2363-2380` (and the `_patch_vt100_parser`
comments around line 2378).

prompt_toolkit's Vt100Parser buffers all input while waiting for a
bracketed-paste terminator; Hermes patches it to recover from torn
sequences. crossterm handles bracketed-paste at the event-decode layer
and emits a single `Event::Paste(String)` — we just need to accept
that variant in the run loop. Today we ignore non-Key events; that's a
five-line addition when we want it.

### D. Patch-stdout protection for background log lines

**Source:** `hermes-agent/cli.py:14640` (`with patch_stdout()`).

This is prompt_toolkit-specific: background `print()` calls would
corrupt the rendered prompt without it. ratatui renders the entire
frame from a buffer every tick, so a stray `tracing::info!` to stderr
just lives behind the alt-screen until the operator exits. The
`init_tracing` in `main.rs` writes to stderr by default for exactly
this reason — the warnings are visible after `q`, never during.

### E. The 15K-line monolith

`cli.py` mixes:
* the chat REPL,
* slash command dispatch,
* skill-aware command discovery,
* a /limits command that lazy-imports the OpenAI SDK (`cli.py:155`,
  cite "transitively pulls the OpenAI SDK chain (~230 ms cold) and is
  only needed when the user runs `/limits`"),
* model-switch UI, voice mode, etc.

That's NOT the architecture for Evy. The Rust workspace already
separates concerns across crates — `evy-chat-tui` should stay focused
on operator chat and delegate everything else (skill catalog, scoring,
playbooks, mandates) to its respective crate. If we end up needing a
status/limits/model panel inside the chat surface, it should be a new
ratatui widget reading from a daemon endpoint, not an inline import.

## Anthropic Messages API differences we don't need to revisit

Hermes supports many providers (Anthropic, OpenAI, Codex, xAI…) via
adapters under `hermes-agent/agent/transports/`. Evy v4 currently
targets Anthropic only via `evy_thinking::AnthropicBackend`. When a
local DGX backend lands ("TODO: Phase 4" markers in `anthropic.rs`),
study `hermes-agent/agent/transports/chat_completions.py` for the
abstraction shape — Hermes already paid the cost of normalising
provider differences and that file is a good reference.

## Headline takeaways

1. **Two-pane layout is uncontroversial** — port Hermes's split as-is.
2. **Newline/submit binding matters** — Alt-Enter newline + Enter
   submit is the operator-friendly choice; Hermes's modify-other-keys
   gymnastics are terminal-specific glue we don't need with crossterm.
