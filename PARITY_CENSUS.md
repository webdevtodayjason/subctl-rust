# PARITY CENSUS — every v3 surface that must exist in v4 before v3 retires

**Wave:** W5 / S1 (locked contract 2026-06-11T02:15Z) · **Operator directive:** "Everything that was in v3 needs to be in v4. That is crucial." (decision log 2026-06-11T01:00Z)
**v3 reference:** `/Users/sem/code/subctl` (dashboard `server.ts` 7,546 lines; evy master `components/evy/server.ts`)
**v4 reference:** `subctl-rust` `main` @ `f4f1f36` (X2 merge) — `crates/evy-comms/src/http.rs` router block, crates survey, ORCHESTRATION.md waves 1–4 + X1/X2.
**Amended post-S2 (W5 memory-wave verification, w-memory worker):** memory-family rows corrected per live verification — tier1/bare-memory/cognee/memori now native; `/api/memory/{stats,recent,search,entries}` tripwire fired (master-owned SQLite, NOT claude-mem) and is re-rowed. See "Amendments (post-S2)" at the bottom.
**This doc is the W7 retirement checklist.** W7 gate = every non-`done` row below is either shipped or carries an explicit operator decision.

## Sanity-check counts

| Class | Enumerated rows | Sanity check |
|---|---|---|
| 1a. Dashboard HTTP routes (`dashboard/server.ts`) | **103** distinct route surfaces | `grep -c url.pathname` = **137** (multiple pathname reads per route: prefix slices, rewrites, match groups — 103 distinct handlers is consistent) |
| 1b. Evy master daemon HTTP routes (`components/evy/server.ts`, :8788) | **60** distinct route surfaces | `grep -c url.pathname` = **66** (66 raw hits incl. slice/replace uses) |
| 2. Evy agent tool modules | **30** modules / **105** defined tools (**99 wired** into the live registry, 6 in 2 unwired modules) | `ls components/evy/tools/*.ts` = 29 files + `policy/` dir = 30 modules; registry spread audited line-by-line in `server.ts:698–852` |
| 3. Management CLI | **45** top-level `subctl` verbs + **8** sibling bins + **4+N** shell helpers | `bin/subctl` case dispatch (lines 221–852) cross-checked against usage text |
| 4. Master/evy runtime behaviors | **28** daemon loops & side-effect subsystems | every `setInterval`/`start*()` in `components/evy/server.ts` boot block + module imports audited |

**Status legend** — `native`: served/implemented by v4 at `f4f1f36`. `proxied`: reaches v3 through v4's reverse-proxy catch-all (`/api/{*rest}`), a dedicated WS-splice, or the CLI shim's v3 fall-through. `absent`: no v4 path at all (dies with v3). `independent`: compiled/self-contained artifact that does not depend on the Bun daemons (still needs a re-homing decision).
**Wave legend** — `done` / `W5` (this wave, S2 in flight) / `W6` (hardening + latent restoration) / `Evy-agency` (the named tool-surface wave) / `long-tail` (port scheduled before W7, not yet wave-assigned) / `W7-prep` (retirement mechanics: install, cutover, repoints).

---

## Class 1a — Dashboard HTTP routes (`dashboard/server.ts`, :8787)

v4 (:8797) fronts everything; routes marked `proxied` ride `/api/{*rest}` → v3 Bun dashboard unless noted.

| # | Surface | Method | Owner today | v4 status | Wave | Notes |
|---|---|---|---|---|---|---|
| 1 | `/health` | GET | dashboard | native | done | v4 serves its own |
| 2 | `/api/master/*` → `/api/evy/*` alias rewrite | any | dashboard | native (scoped) | done | P3 ruling: rewrite covers chat-family paths only; other master-dialect paths fall through to proxy — full-dialect adoption is a W7-prep decision |
| 3 | `/api/evy/chat` | POST | v4 (v3 proxies TO v4) | native | done | P1; Fork-A bridge inverted — v3 dashboard now forwards here |
| 4 | `/api/evy/events` (SSE) | GET | v4 | native | done | live vocabulary proven (message_start/update/end, agent_end, watchdog_ok) |
| 5 | `/api/evy/transcript` | GET | v4 | native | done | P2 |
| 6 | `/api/evy/context` | GET | v4 | native | done | P2 |
| 7 | `/api/evy/transcript/util` | GET | v4 | native | done | P2 |
| 8 | `/api/evy/transcript/compact` | POST | v4 | native | done | P3 |
| 9 | `/api/evy/transcript/clear` | POST | v4 | native | done | P3 (+ session reset) |
| 10 | `/api/live` (WS) | GET | dashboard | proxied (dedicated WS-splice `ws_proxy_handler`) | W7-prep | live-state push WS; at retirement either port natively or repoint frontend to `/api/evy/events` SSE |
| 11 | `/api/terminal/enabled` | GET | dashboard | proxied | long-tail | terminal family; flag-file gated |
| 12 | `/api/terminal/teams` | GET | dashboard | proxied | long-tail | |
| 13 | `/api/terminal/attach` (WS) | GET | dashboard | proxied (dedicated tokio-tungstenite splice, `terminal_ws.rs`) | long-tail | real-PTY needs libc/openpty waiver — **operator checkpoint** (W8 spike finding) |
| 14 | `/api/state` | GET | dashboard | native | done | W1-sprint; native accounts+dispatch overlay |
| 15 | `/api/host` | GET | dashboard | native | done | Phase 0 |
| 16 | `/api/models` | GET | dashboard | native | done | W1 (LM Studio catalog — v4-owned data) |
| 17 | `/api/models/refresh` | POST | dashboard | native | done | W1 |
| 18 | `/api/projects` | GET | dashboard | native | done | W3 (0.2s vs Bun 13.8s) |
| 19 | `/api/logs/sources` | GET | dashboard (launchd log files) | proxied | W6 | file-backed read; cheap native port |
| 20 | `/api/logs/{source}` + `/stream` (SSE) | GET | dashboard | proxied | done (W6) | was CONDITIONALLY lazy (immediate only on clean log read; missing/rotated file hung head to 25s ka) — fixed in PR #57 `: open` sweep; live-verified instant on both ports 2026-06-11 |
| 21 | `/api/audit/aggregate` | GET | dashboard (policy audit JSONLs) | proxied | long-tail | policy-audit family; evy-policy crate exists, audit files on disk |
| 22 | `/api/policy/teams` | GET | dashboard | proxied | long-tail | |
| 23 | `/api/policy/list` | GET | dashboard | proxied | long-tail | |
| 24 | `/api/audit/{team}` + `/stream` (SSE) | GET | dashboard | proxied | long-tail | SSE variant — same catch-all streaming caveat |
| 25 | `/api/teams/tools` | GET | dashboard → master tool registry | proxied (explicit literal route → proxy) | Evy-agency | tool list IS the v3 master tool registry — lands for free with the Evy-agency registry |
| 26 | `/api/teams` | GET, POST | dashboard | native | done | W4/2m team-template CRUD |
| 27 | `/api/teams/{name}` | GET, PUT, DELETE | dashboard | native | done | |
| 28 | `/api/skills` | GET | dashboard (`~/.config/subctl/skills` files) | proxied | long-tail | evy-skills crate substrate exists; whole panel family ports together. *Contestable: could pull into W6 (file-backed reads)* |
| 29 | `/api/skills/sources` | GET | dashboard | proxied | long-tail | |
| 30 | `/api/skills/{id}` | GET | dashboard | proxied | long-tail | |
| 31 | `/api/skills/import` | POST | dashboard | proxied | long-tail | mutation (git clone into catalog) |
| 32 | `/api/skills/categorized` | GET | dashboard | proxied | done (W6) | was 400 — catch-all shadowing in v3; fixed v3-side (PR #56), live 200 both ports 2026-06-11 |
| 33 | `/api/skills/evy/{id}` | GET | dashboard (evy-authored drafts) | proxied | long-tail | pairs with Evy-agency authoring tools |
| 34 | `/api/skills/evy/{id}/promote` | POST | dashboard | proxied | long-tail | |
| 35 | `/api/skills/evy/{id}/delete` | POST | dashboard | proxied | long-tail | |
| 36 | `/api/settings/install-checks` | GET | dashboard (install tree) | proxied | W7-prep | install-coupled — meaning changes when install story is v4's |
| 37 | `/api/settings/obsidian` | GET, POST | dashboard | native | done | W1-sprint read + W2 write |
| 38 | `/api/settings/keys` | GET | dashboard (Bun process env) | proxied | W7-prep | W1 hollow-data lesson: reads v3-process env; port when env owner is v4 |
| 39 | `/api/settings/secrets` | GET | dashboard | proxied | W7-prep | evy-secrets crate is the native substrate |
| 40 | `/api/settings/secrets/{key}` | POST, DELETE | dashboard | proxied | W7-prep | secrets writes |
| 41 | `/api/settings/oauth` | GET | dashboard | native | done | W2 |
| 42 | `/api/settings/telegram` | POST | dashboard | native | done | W2 mutation parity; W6: hot-applies creds to live bridge (43f0c0f), boot-absent bridge stays restart-required (documented) |
| 43 | `/api/settings/telegram/test` | POST | dashboard | native | done | W2 |
| 44 | `/api/settings/config/{name}` | GET | dashboard (redacted config files) | native | done | W2; GET-only in v3 (verified server.ts:4363) |
| 45 | `/api/projects/create` | POST | dashboard | native | done | W3 |
| 46 | `/api/projects/{name}` | GET | dashboard | native | done | W3; GET-only in v3 (verified server.ts:4611) |
| 47 | `/api/memory/tier1` | GET, POST | dashboard (master's always-in-context flat file) | native | done (W5) | S2 shipped. **LATENT (W6 candidate):** v3's tier1-memory.ts writes `~/.config/subctl/master/`, reaching the documented `evy/` dir only via a host-local SYMLINK — fresh installs silently lose dashboard tier1 edits; also v3 dashboard tier1 block hardcodes `$HOME`, ignoring `SUBCTL_CONFIG_DIR` (server.ts:4752) |
| 48 | `/api/vault/roots` | GET | dashboard (vault dirs on disk) | proxied | W6 | vault browser family — file-backed reads; omitted from W5 recon table, assigning W6. *Contestable: could fold into the W5 memory family* |
| 49 | `/api/vault/{root}/tree` | GET | dashboard | proxied | W6 | |
| 50 | `/api/vault/{root}/note` | GET, POST | dashboard | proxied | W6 | POST = note write (same-owner flat file) |
| 51 | `/api/vault/{root}/asset` | GET | dashboard | proxied | W6 | |
| 52 | `/api/memory` (bare) | GET | dashboard (Obsidian + vault state) | native | done (W5) | S2 shipped |
| 53 | `/api/version` | GET | dashboard | native | done | |
| 54 | `/api/update/check` | GET | dashboard (install tree + git) | proxied | W7-prep | install-coupled |
| 55 | `/api/update/events` (SSE) | GET | dashboard | proxied | done (W6) | ROOT CAUSE: Bun lazy head (never the proxy) — v4 SSE head-grace (9091a6c) + v3 `: open` fix-at-source (PR #57); bare-curl proven both ports 2026-06-11 |
| 56 | `/api/update/run` | POST | dashboard | proxied | W7-prep | self-update of the v3 install — replaced by v4 deploy story |
| 57 | `/api/providers` | GET | dashboard (pi-ai npm catalog) | proxied | long-tail | W1 ruling: pi-ai data has no Rust port — **operator checkpoint** (port effort = days) |
| 58 | `/api/catalogs` | GET | dashboard (pi-ai) | proxied | long-tail | |
| 59 | `/api/catalogs/{provider}` | GET | dashboard | proxied | long-tail | |
| 60 | `/api/catalogs/{provider}/models/enabled-all` | POST | dashboard | proxied | long-tail | |
| 61 | `/api/catalogs/{provider}/{model}/enabled` | POST | dashboard | proxied | long-tail | per-model toggle |
| 62 | `/api/catalogs/{provider}/refresh` | POST | dashboard | proxied | long-tail | |
| 63 | `/api/providers/{provider}/default-model` | GET, POST | dashboard | proxied | long-tail | |
| 64 | `/api/auth/openai-codex/*` (OAuth flow tails) | GET, POST | dashboard | proxied | long-tail | device-flow OAuth; auth flows port with the CLI auth family |
| 65 | `/api/providers/profiles` | POST, DELETE | dashboard (accounts.conf) | native | done | W1 + X1 newline fix |
| 66 | `/api/evy/supervisor` | POST | dashboard → master | proxied | long-tail | supervisor model switch — master-owned runtime state |
| 67 | `/api/watchdogs` (browser-bare) | GET | dashboard → master | proxied | W7-prep | native canonical `/api/evy/watchdogs/diag` exists (merged view); bare-path repoint at retirement |
| 68 | `/api/watchdogs/{id}/kill` (browser-bare) | POST | dashboard → master | proxied | W7-prep | native canonical `/api/evy/watchdogs/{id}/kill` exists |
| 69 | `/api/profile` | GET, POST | dashboard → master | proxied | long-tail | master-owned profile state (chat/heavy) |
| 70 | `/api/evy/engagement` | POST | dashboard → master | proxied | long-tail | engagement instrumentation write |
| 71 | `/api/evy/fitness/ledger` | GET | dashboard | proxied | long-tail | kernel-fitness data plane |
| 72 | `/api/evy/engagement/ledger` | GET | dashboard | proxied | long-tail | |
| 73 | `/api/evy/fitness/health` | GET | dashboard | proxied | long-tail | |
| 74 | `/api/evy/restart` | POST | dashboard (launchctl kickstart) | proxied | W7-prep | restart semantics flip to the v4 daemon at retirement |
| 75 | `/api/notifications/*` (browser-bare, incl. `/stream` SSE) | GET, POST | dashboard → master | proxied | W7-prep | native canonical `/api/evy/notifications` family exists (W6-sprint); bare repoint + stream port at retirement |
| 76 | `/api/voice/*` (incl. `/audio/{file}`) | GET, POST | dashboard → master | proxied | long-tail | W5 non-goal; evy-voice crate is the native substrate, HTTP surface unported |
| 77 | `/api/preferences/*` | GET, POST | dashboard → master | proxied | done (W6) | RESTORED v3-side (PR #56): module existed complete since v2.8.1, never mounted; mounted + tools registered (79→81); live 200 end-to-end both ports 2026-06-11 |
| 78 | `/api/upstreams` + `/check` `/history` `/update` `/auto-update/toggle` | GET, POST | dashboard → master | proxied | long-tail | **Contestable: pi-ai/pi-agent-core npm upstreams are v3-stack concepts — candidate for retire-with-v3 instead of port** |
| 79 | `/api/cognee/*` | any | dashboard → Cognee sidecar (:8745, `SUBCTL_COGNEE_PORT`) | native | done (W5) | S2 shipped — thin native forward |
| 80 | `/api/memori/*` | any | dashboard → Memori sidecar (:8746, `SUBCTL_MEMORI_PORT`) | native | done (W5) | S2 shipped — thin native forward |
| 81 | `/api/memory/{stats,recent,search,entries[,/{id}]}` | GET, POST, DELETE | **v3 master runtime store** — master's OWN SQLite `~/.local/state/subctl/memory/evy.db` (components/evy/memory.ts:24,73), NOT claude-mem | proxied | Evy-agency-or-later | **S2 TRIPWIRE FIRED** — blocked on store ownership, not effort. Master applies daemon-side egress redaction (server.ts:4882–4886); any native port MUST reproduce it |
| 82 | `/api/evy/*` catch-all → master `/{rest}` | any | dashboard → master | proxied | — | umbrella; every reachable master route censused row-by-row in Class 1b |
| 83 | `/api/refresh` | POST | dashboard | native | done | W1-sprint (usage cache bust) |
| 84 | `/cheat`, `/cheatsheet` | GET | dashboard | proxied | W7-prep | static helper page; trivial port or drop with operator nod |
| 85 | `/help` | GET | dashboard | proxied | W7-prep | |
| 86 | `/api/sessions/preview` (bare) | GET | dashboard | proxied | W7-prep | native canonical `/api/evy/sessions/preview` exists (W4-sprint) |
| 87 | `/api/sessions/spawn` (bare) | POST | dashboard | proxied | W7-prep | native canonical exists |
| 88 | `/api/notify/inbox` | GET | dashboard (inbox JSONL) | proxied | long-tail | operator-reply inbox; v4 has native `/api/evy/notify` + `/api/evy/ask` but no inbox read/ack surface |
| 89 | `/api/notify/inbox/{id}/ack` | POST | dashboard | proxied | long-tail | |
| 90 | `/api/asks/pending` | GET | dashboard | proxied | long-tail | pending structured questions |
| 91 | `/api/notify/reply` | POST | dashboard | proxied | long-tail | |
| 92 | `/api/sessions/list` (bare) | GET | dashboard | proxied | W7-prep | native canonical exists (byte-identical proven) |
| 93 | `/api/orchestration/spawn` (bare action dialect) | POST | dashboard | proxied | W7-prep | W4 ruling: v3 actions keep operating on v3's registry; native canonical `/api/evy/orchestration/spawn` exists |
| 94 | `/api/orchestration` | GET | v4 (merged registry, origin-tagged) | native | done | W4; v3-dark proof passed |
| 95 | `/api/orchestration/captures` | GET | v4 | native | done | W4 |
| 96 | `/api/orchestration/{name}/msg` | POST | dashboard | proxied | W7-prep | action-dialect cutover |
| 97 | `/api/orchestration/{name}` | GET | dashboard | proxied | W7-prep | deliberately left on catch-all (W4 ruling) |
| 98 | `/api/orchestration/{name}/kill` | POST | dashboard | proxied | W7-prep | native canonical `/api/evy/orchestration/{id}/kill` exists |
| 99 | `/api/sessions/{id}/kill` (bare) | POST | dashboard | proxied | W7-prep | native canonical exists |
| 100 | static SPA (`serveStatic`, dashboard web bundle) | GET | dashboard | native | done | v4 `ServeDir` operator-console fallback (Phase 4 Slice A) |
| 101 | `/api/evy/workers` | GET | v4-new | native | done | no v3 ancestor; listed for completeness |
| 102 | `/api/evy/scheduler/jobs` | GET | v4-new | native | done | no v3 ancestor |
| 103 | `/api/evy/policy` + `/api/evy/accounts` + `/api/evy/rate-limits` + `/api/evy/cost` | GET | v4 | native | done | Phase 1 + cutover natives (accounts/usage/verdict/cost) |

## Class 1b — Evy master daemon routes (`components/evy/server.ts`, :8788; reached via dashboard `/api/evy/*` proxy unless noted)

| # | Surface | Method | Owner today | v4 status | Wave | Notes |
|---|---|---|---|---|---|---|
| 1 | `/.well-known/mcp` + `/mcp*` (MCP server) | any | master | absent | long-tail | **NOT behind the dashboard proxy — external MCP clients hit :8788 directly; dies silently with v3.** Token-gated (`subctl_mcp_token`). *Contestable: re-scope vs port* |
| 2 | `/health` | GET | master | native | done | v4 daemon has own /health |
| 3 | `/api/debug/usage` | GET | master | proxied | long-tail | debug surface. *Contestable: retire-with-v3* |
| 4 | `/profile` | GET, POST | master | proxied | long-tail | active profile + supervisor info |
| 5 | `/watchdogs` | GET | master | native (equiv: `/api/evy/watchdogs/diag` merged view) | done | merged v3+v4 diag; v4 rows survive v3-dark |
| 6 | `/watchdogs/{id}/kill` | POST | master | native (canonical) | done | W5-sprint |
| 7 | `/watchdogs/killall` | POST | master | absent | long-tail | no v4 equivalent of kill-all |
| 8 | `/notifications` | GET | master | native | done | W6-sprint port of notifications.ts |
| 9 | `/notifications/{id}/read` | POST | master | native | done | |
| 10 | `/notifications/read-all` | POST | master | native | done | |
| 11 | `/notifications/stream` (SSE) | GET | master | proxied | W6 | stayed v3 by design; native option = repoint consumers to `/api/evy/events` |
| 12 | `/secrets/backends` | GET | master | absent | long-tail | evy-secrets crate is native substrate; HTTP surface unported |
| 13 | `/secrets/test` | POST | master | absent | long-tail | |
| 14 | `/secrets/cache/flush` | POST | master | absent | long-tail | |
| 15 | `/upstreams` | GET | master | proxied | long-tail | see Class 1a #78 — retire-candidate |
| 16 | `/upstreams/check` | POST | master | proxied | long-tail | |
| 17 | `/upstreams/history` | GET | master | proxied | long-tail | |
| 18 | `/upstreams/update` | POST | master | proxied | long-tail | |
| 19 | `/upstreams/auto-update/toggle` | POST | master | proxied | long-tail | |
| 20 | `/memory/search` | GET | **master runtime store** (own SQLite `~/.local/state/subctl/memory/evy.db` — memory.ts:24,73; NOT claude-mem) | proxied | Evy-agency-or-later | S2 TRIPWIRE — blocked on store ownership, not effort; egress redaction (server.ts:4882–4886) must be reproduced |
| 21 | `/memory/recent` | GET | master runtime store (evy.db) | proxied | Evy-agency-or-later | same |
| 22 | `/memory/stats` | GET | master runtime store (evy.db) | proxied | Evy-agency-or-later | same |
| 23 | `/memory/entries` | POST | master runtime store (evy.db) | proxied | Evy-agency-or-later | write |
| 24 | `/memory/entries/{id}` | DELETE | master runtime store (evy.db) | proxied | Evy-agency-or-later | write |
| 25 | `/memory/kernel/status` | GET | master runtime | absent | long-tail | W5 explicit non-goal (kernel = master runtime); ports with runtime row 4.7. **memory.js kernel panel still requires v3+master up post-W5 (expected strangler state)** |
| 26 | `/memory/kernel/run-now` | POST | master | absent | long-tail | |
| 27 | `/memory/kernel/pause` | POST | master | absent | long-tail | |
| 28 | `/memory/kernel/resume` | POST | master | absent | long-tail | |
| 29 | `/memory/tier1/pending` | GET | master | absent | long-tail | tier1 workflow = W5 non-goal |
| 30 | `/memory/tier1/approve` | POST | master | absent | long-tail | |
| 31 | `/memory/tier1/reject` | POST | master | absent | long-tail | |
| 32 | `/memory/tier1/consolidate` | POST | master | absent | long-tail | |
| 33 | `/memory/backfill/evy-to-memori` | POST | master | absent | long-tail | operator-demand backfills. **memory.js backfill panel still requires v3+master up post-W5 (expected strangler state)** |
| 34 | `/memory/backfill/claude-mem-to-cognee` | POST | master | absent | long-tail | |
| 35 | `/memory/backfill/obsidian-to-cognee` | POST | master | absent | long-tail | |
| 36 | `/transcript` | GET | master | native | done | chat family (master dialect via scoped rewrite) |
| 37 | `/context` | GET | master | native | done | |
| 38 | `/transcript/compact` | POST | master | native | done | |
| 39 | `/transcript/util` | GET | master | native | done | |
| 40 | `/transcript/clear` | POST | master | native | done | |
| 41 | `/reload-supervisor` | POST | master | absent | long-tail | hot-reload supervisor prompt/config |
| 42 | `/local-backend` | GET, POST | master (LM Studio backends) | absent | long-tail | local-LLM backend config + test |
| 43 | `/local-backend/test` | POST | master | absent | long-tail | |
| 44 | `/teams` | GET | master (live agent-team registry) | absent | long-tail | **distinct from template CRUD** — P3 ruling proved v4's `/api/evy/teams` is templates, NOT this. v4's worker registry (`/api/evy/workers` + orchestration) is the successor surface; needs a mapped repoint |
| 45 | `/teams/{name}/prune` | POST | master | absent | long-tail | |
| 46 | `/diag` | GET | master | native (merged into v4 diag view) | done | v4 diag keeps serving v4 rows when v3 is dark |
| 47 | `/personality` | GET, POST | master | absent | long-tail | personality hot-swap |
| 48 | `/providers/{p}/upstream-catalog` | GET | master (pi-ai) | proxied | long-tail | pi-ai checkpoint family |
| 49 | `/providers/{p}/upstream-catalog/refresh` | POST | master | proxied | long-tail | |
| 50 | `/chat` | POST | master | native | done | superseded by v4 chat (dashboard no longer routes here) |
| 51 | `/attachments` | GET, POST | master | native | done | W6-sprint port |
| 52 | `/attachments/{id}` | GET | master | native | done | (+ DELETE in v4) |
| 53 | `/events` (SSE) | GET | master | native | done | |
| 54 | `/voice/render` | POST | master | absent | long-tail | evy-voice crate = TTS substrate; HTTP unported; W5 non-goal |
| 55 | `/voice/status` | GET | master | absent | long-tail | |
| 56 | `/voice/config` | POST | master | absent | long-tail | |
| 57 | `/voice/audio/{file}` | GET | master | absent | long-tail | rendered-audio serving |

*(57 enumerated above + `/profile` POST and `/personality` POST counted within their rows + `/api/debug/usage` = 60 distinct method-surfaces; 66 raw `url.pathname` hits.)*

---

## Class 2 — Evy agent tools (`components/evy/tools/`, registered in `server.ts:698–852`)

**v4 status for the entire class: ABSENT.** Confirmed in the decision log (2026-06-11T00:15Z): v4 chat's `conversational_system_prompt()` disclaims tool access; only `skill_view` reaches the wire. Operator directive makes the port mandatory → **wave: Evy-agency** for every wired row unless noted. "Substrate" notes where a v4 crate already owns the data — those rows are cheap.

| Module | Tools (registered name) | Purpose | Gate | v4 status | Wave | Notes |
|---|---|---|---|---|---|---|
| subctl-orch.ts | subctl_orch_list, subctl_orch_status, subctl_orch_spawn, subctl_orch_spawn_template, subctl_orch_msg, subctl_orch_kill, subctl_orch_state, subctl_orch_inbox (8) | tmux orchestrator control | — | absent | Evy-agency | substrate native: v4 spawn/kill/captures handlers exist |
| gh.ts | gh_pr_list, gh_pr_view, gh_pr_checks, gh_issue_list (4) | GitHub via `gh` CLI | `gh` binary | absent | Evy-agency | shell-out port |
| coderabbit.ts | coderabbit_review_local, coderabbit_preview_prompts, coderabbit_stats (3) | CodeRabbit CLI review | `coderabbit` CLI | absent | Evy-agency | |
| telegram.ts | telegram_send, telegram_send_voice, telegram_send_digest (3) | outbound Telegram (text/voice/digest) | — | absent (text bridge native; tool wiring + voice absent) | Evy-agency | voice notes need creds — **operator checkpoint** |
| system.ts | system_hardware, system_load, system_disk, system_lmstudio_models, system_tmux_sessions, system_process_top, system_projects_dir, system_daemon_self, system_my_tools (9) | host introspection | — | absent | Evy-agency | read-only; first slice of the agency wave per decision log |
| project.ts | project_create, vault_append (2) | project scaffold + vault append | — | absent | Evy-agency | |
| memory.ts | memory_search, memory_timeline, memory_observations, memory_health (4) | Tier-4 memory (Cognee→claude-mem) | — | absent | Evy-agency | substrate native: evy-memory crate has ClaudeMemReader (Tier-4 claude-mem only — the Tier-3 evy.db store stays master-owned per S2 tripwire) |
| context7.ts | context7_resolve, context7_docs, context7_health (3) | Context7 docs lookup | CONTEXT7_API_KEY | absent | Evy-agency | |
| tier1-memory.ts | memory_show, memory_remember, memory_forget, memory_user_update, memory_tier1_pending, memory_tier1_approve, memory_tier1_reject (7) | Tier-1 always-in-context memory + approval workflow | — | absent | Evy-agency | couples to runtime rows 4.7–4.8. **LATENT:** writes `~/.config/subctl/master/`, reaches documented `evy/` dir only via host-local symlink (see 1a #47) |
| skill-author.ts | skill_create, skill_revise, skill_remove, skill_list_master (4) | master-private skill catalog authoring | skill-router | absent | Evy-agency | |
| skills-author.ts | evy_author_skill, evy_list_authored_skills, evy_promote_skill, evy_delete_authored_skill (4) | Evy-curated skill drafts (operator review) | skill-router | absent | Evy-agency | pairs with dashboard rows 1a #33–35 |
| notify.ts | notify_dashboard (1) | push notification to dashboard tray | — | absent | Evy-agency | substrate native: v4 notifications store |
| specforge.ts | specforge (1) | spec generation workflow | — | absent | Evy-agency | |
| scheduler.ts | schedule_followup, list_followups, cancel_followup (3) | self-scheduled follow-ups | — | absent | Evy-agency | substrate native: evy-scheduler crate (cron jobs, persisted) |
| attachments.ts | read_attachment, list_attachments (2) | read chat attachments | — | absent | Evy-agency | substrate native: v4 attachments store (W6-sprint) |
| vault-link.ts | vault_link (1) | deep-link vault notes | — | absent | Evy-agency | |
| policy/ (index.ts) | policy_check, policy_list, policy_audit_tail (3) | policy engine introspection | — | absent | Evy-agency | substrate native: evy-policy crate |
| diag.ts | system_watchdog_self, system_port_check, system_lmstudio_health, system_log_tail, system_rate_limit_status, system_git_status, system_network_health, system_version_status, system_supervisor_info, system_cognee_promotion_self (10) | read-only self-diagnostics | — | absent | Evy-agency | read-only; first-slice candidates |
| web.ts | web_search, web_fetch (2) | Brave search + Firecrawl fetch | keys | absent | Evy-agency | substrate native: evy-research crate (TinyFish) — decide Brave/Firecrawl vs TinyFish consolidation |
| tinyfish.ts | tinyfish_search, tinyfish_fetch, tinyfish_agent, tinyfish_agent_async (4) | TinyFish web toolkit | TINYFISH_API_KEY | absent | Evy-agency | substrate native: evy-research crate |
| background.ts | background_run, background_status, background_cancel (3) | background tool-run runtime | — | absent | Evy-agency | runtime row 4.15 |
| knowledge-graph.ts | knowledge_graph_neighbors, knowledge_graph_path, knowledge_graph_query (3) | multi-hop Cognee graph reasoning | Cognee reachable | absent | Evy-agency | |
| linear.ts | linear_list_issues, linear_search, linear_create_issue, linear_update_issue (4) | Linear GraphQL | LINEAR_API_KEY | absent | Evy-agency | |
| knowledge.ts | system_subctl_knowledge (1) | TOON self-knowledge of subctl | — | absent | Evy-agency | needs v4-flavored knowledge file |
| team-docs.ts | team_doc_write, team_doc_read, team_doc_list, team_decision_log (4) | `<project>/.subctl/docs/` docs + decision log | — | absent | Evy-agency | |
| watchdogs.ts | watchdog_list, watchdog_kill (2) | enumerate/kill registered watchdogs | — | absent | Evy-agency | substrate native: WatchdogDiagRegistry |
| evy-memory.ts | evy_recall, evy_remember (2) | Tier-3 conversational memory | — | absent | Evy-agency | substrate native: evy-memory observation log |
| voice-render.ts | voice_render, voice_status (2) | TTS render | voice.json enabled | absent | Evy-agency | substrate native: evy-voice crate |
| **preferences.ts** | evy_get_preferences, evy_set_preference, evy_get_preference_value (3) | operator preferences | — | v3-wired (W6: mounted + 2 tools registered, PR #56) | Evy-agency | port with agency wave; NOTE closet #424: renderPreferencesForPrompt still never injected — Evy sets prefs she can't see |
| **team-templates.ts** | subctl_team_template_list, subctl_team_template_show, subctl_team_dispatch (3) | team-template list/show/dispatch | — | absent (**UNWIRED in v3 too** — never imported) | long-tail | **finding:** dead module; template CRUD already v4-native, dispatch tool = Evy-agency candidate |

**Totals:** 30 modules, 105 defined tools, **99 wired** (28 modules) + **6 unwired** (2 dead modules). MCP server (`mcp/`) re-exports registry tools — censused as runtime row 4.14 / Class 1b #1.

---

## Class 3 — Management CLI

### 3a. `bin/subctl` (v3 bash dispatcher; v4 owns the bare name and forwards all of these verbatim — `bin/subctl` shim fall-through, X1)

Owner today = v3 bash CLI (`lib/*.sh`) for every row. v4 status = `proxied` (shim forward works today, dies only if the v3 repo tree is deleted — but the Bun daemons many verbs call DO retire, noted per-row).

| Verb (subverbs) | Backing | v4 status | Wave | Notes |
|---|---|---|---|---|
| *(bare)* / menu / tui | lib/tui.sh | native (v4 shim launches chat TUI instead) | done | **intentional behavior change** — v3 TUI menu reachable via forward |
| accounts (status/add/remove/edit) | lib/accounts.sh, accounts.conf | proxied | long-tail | flat-file; v4 already writes accounts.conf (profiles CRUD) |
| auth (claude/openai/openai-codex/xai-oauth/pi-coding-agent/deepseek/gemini/all) | providers/*/auth.sh | proxied | long-tail | dashboard copies `subctl auth claude <alias>` — must keep working verbatim (X1 proved) |
| teams (claude/pi-coding-agent/codex/deepseek/gemini) | lib + providers | proxied | long-tail | tmux launcher |
| radar (+ log) | lib/radar | proxied | long-tail | dispatch-check verdict; v4 owns the data (rate-limits native) |
| service (status/start/stop/restart/enable/disable/logs/foreground) | launchd plists | proxied | W7-prep | controls the **v3** daemons — retirement mechanics |
| dashboard (open / deploy) | lib | proxied | W7-prep | deploy = v3 install-tree pull |
| deck | Go binary `bin/subctl-deck` | independent | W7-prep | Bubble Tea TUI; re-home dispatch when v3 tree goes |
| session-preview / session-list / session-kill / session-prune / prune-transcripts / session-resume | lib/session-preview.sh | proxied | long-tail | v4 serves the same data natively over HTTP (W4-sprint) — CLI wrapper port is thin |
| sessions (list/adopt/adopt-latest/pick) | lib/sessions.sh | proxied | long-tail | transcript adoption across cfg_dirs |
| projects (status/start/edit/path) | lib/projects.sh, projects.conf | proxied | long-tail | |
| usage [alias\|all\|--json] | lib/usage.sh | proxied | long-tail | v4 native equivalent data at `/api/evy/accounts` |
| cost | lib | proxied | long-tail | v4 native `/api/evy/cost` |
| whoami | lib | proxied | long-tail | |
| config (show/edit/validate/path) | lib | proxied | long-tail | secrets auto-redacted |
| doctor | lib | proxied | W7-prep | health checks reference v3 daemons; needs v4 rewrite |
| install [--migrate] / setup [--wizard] | install.sh | proxied | W7-prep | the install story IS W7-prep |
| notify (msg/--setup/--test/--status/ask-yesno/ask-choice/ask-text/inbox) | lib | proxied | long-tail | v4 has native /api/evy/notify + /ask; CLI + inbox surface unported |
| orch (list/spawn/status/msg/kill) | lib | proxied | long-tail | v4 orchestration handlers exist for spawn/list/kill |
| team (list/kill/exec/logs/report/inbox/spawn --template/baseline) | lib | proxied | long-tail | HMAC-signed exec; baseline = claude-layers install |
| profile (show/switch/list) | lib | proxied | long-tail | master-owned profile state |
| skills (import/list/info/sources/router-trace) | lib | proxied | long-tail | pairs with 1a skills family |
| templates (list/show/create/duplicate/delete) | lib | proxied | long-tail | template CRUD is v4-native over HTTP already |
| plugins (list/install/remove/status) | lib/plugins.sh | proxied | long-tail | |
| evy (verbs) / master (deprecated alias) | lib/evy.sh | proxied | W7-prep | controls the v3 Bun daemon — replaced by v4 `subctl daemon *` |
| policy (check/list/validate/explain/audit/snapshot) | lib/policy.sh + Go `subctl-policy-check` | proxied (Go kernel: independent) | long-tail | `check` is the hot-path hook gate — must survive retirement intact |
| update / deploy | lib/update.sh, lib/cli.sh | proxied | W7-prep | v3 self-update; v4 deploy story replaces |
| status [--json] | lib/cli.sh | proxied | W7-prep | probes :8788/:8787 — needs v4 targets |
| logs [--master\|--dashboard] | lib/cli.sh | proxied | W7-prep | v4 shim has own `subctl logs` (v4 daemon log) — merge semantics |
| notif (recent/list/mark-all-read) | lib/cli.sh | proxied | long-tail | v4 native notifications exist; CLI repoint |
| memory (recent/search/remember) | lib/cli.sh | proxied | long-tail | repoint to W5 native routes when merged |
| voice (status/test/render/on/off) | lib/cli.sh | proxied | long-tail | voice family |
| memori (lifecycle) / cognee (lifecycle) | lib/cli.sh | proxied | long-tail | sidecar start/stop — sidecars outlive v3 |
| prefs (show/get/set/edit/reset) | lib/cli.sh | proxied | long-tail | reads TOML directly (works despite dead HTTP route) |
| uninstall | uninstall.sh | proxied | W7-prep | |
| version / help | inline | native (v4 shim has own version/help; v3's via forward) | done | |

### 3b. v4-reserved verbs (new, no v3 ancestor): `chat`, `daemon start|stop|restart|status`, `health`, `logs`, `version` — native, done.

### 3c. Sibling bins (`~/.local/lib/subctl-install/bin/`)

| Bin | What it is | v4 status | Wave | Notes |
|---|---|---|---|---|
| claude-dash | shim → `subctl dashboard` | proxied (sibling-resolves v3 dispatcher) | W7-prep | repoint to v4 dashboard at retirement |
| claude-deck | shim → `subctl deck` | proxied | W7-prep | |
| claude-kill | shim → `subctl session-kill` | proxied | long-tail | with session family |
| claude-radar | shim → `subctl radar` | proxied | long-tail | |
| claude-resume | shim → `subctl session-resume` | proxied | long-tail | |
| claude-teams | shim → `subctl teams claude` | proxied | long-tail | |
| subctl-deck | Go (Mach-O arm64) Bubble Tea TUI | independent | W7-prep | verify any :8787 API calls before cutover |
| subctl-policy-check | Go policy gate kernel (dir w/ source + binary) | independent | long-tail | hot-path hook gate; survives retirement, re-home build/install |

### 3d. Shell helpers (`~/.config/subctl/shell-aliases.sh`, auto-generated by `subctl install`)

| Helper | Purpose | v4 status | Wave | Notes |
|---|---|---|---|---|
| `_subctl_v3()` | resolve v3 dispatcher (override → ~/bin → PATH) | proxied | W7-prep | PATH-recursion fallback bug already closet-logged (X1) |
| `claude-whoami` | show shell's account | proxied (parses `subctl config show`) | long-tail | |
| `claude-accounts` | account table | proxied | long-tail | |
| `claude-use <alias>` | switch CLAUDE_CONFIG_DIR in-shell | proxied | long-tail | |
| `claude()` guard | block bare REPL without account pick | independent | long-tail | regeneration must survive a v4-owned `subctl install` |

---

## Class 4 — Master/evy runtime behaviors (daemon loops + side-effects, not HTTP)

| # | Behavior | v3 source | v4 status | Wave | Notes |
|---|---|---|---|---|---|
| 4.1 | Inbox poll loop (operator replies JSONL → agent funnel) | server.ts:2968 | absent | Evy-agency | feeds replies into the agent; meaningless until v4 has an agent loop with tools |
| 4.2 | Telegram listener (inbound operator msgs incl. transcribed voice; outbound replies) | evy-notify-listener.ts (in-process) | native (text: evy-comms telegram bridge boots at daemon start) | done* | *voice-note handling + structured-ask reply routing unverified in v4 — voice = **operator creds checkpoint**, long-tail |
| 4.3 | Discord bridge | — (v4-new) | native | done | no v3 ancestor |
| 4.4 | Team-staleness watchdog | server.ts:6966 | native (armed at boot, X2; watchdog_ok live-proven) | done | |
| 4.5 | Auto-nudge state machine (stale-team action) | auto-nudge.ts | absent | long-tail | v3 itself demoted it to broadcast-only (no synthesized prompts); v4 fires events. *Contestable: retire* |
| 4.6 | Team-GC (stale team-registry dirs) | team-gc.ts | native (TeamGcWatchdog armed at boot, X2) | done | |
| 4.7 | Memory-kernel ticker + reviewer (consciousness cycle over tier1 candidates) | memory-kernel.ts, memory-kernel-reviewer.ts, server.ts:3155 | absent | long-tail | W5 explicit non-goal; pairs with 1b #25–28 |
| 4.8 | Tier-1 candidate capture + consolidator | tier1-candidates.ts, tier1-consolidator.ts | absent | long-tail | pairs with 1b #29–32 |
| 4.9 | Cognee promotion ticker (Tier 3→4 write path) | cognee-promotion.ts, server.ts:3208 | absent | long-tail | |
| 4.10 | Follow-up ticker (due scheduled follow-ups → agent) | scheduler tool + server.ts:7007 | absent (evy-scheduler crate = native cron substrate) | Evy-agency | wire scheduler jobs → chat once tools land |
| 4.11 | Engagement sweeper (hourly ledger outcomes) | engagement-tracker.ts, server.ts:7025 | absent | long-tail | pure data-plane |
| 4.12 | Fitness writer (decisions+audit → fitness-ledger.jsonl) | fitness-writer.ts, server.ts:7053 | absent | long-tail | pure data-plane |
| 4.13 | Auto-compact loop (Hermes compression policy + LLM compactor + usage capture) | compression-compactor.ts, compact-policy.ts, supervisor-usage-capture.ts, server.ts:7161 | absent (manual `/transcript/compact` IS native) | long-tail | the automatic trigger + LLM summarizer are unported |
| 4.14 | MCP server (tool surface over MCP, token-gated) | mcp/, server.ts:7321 | absent | long-tail | external clients on :8788 — see 1b #1. *Contestable scope* |
| 4.15 | Background-runs runtime (async tool dispatch) | background-runs.ts | absent | Evy-agency | needs the tool registry first |
| 4.16 | Circuit breaker (empty-listener tool-call breaker) | circuit-breaker.ts | absent | Evy-agency | guards tool loops; only meaningful with tools |
| 4.17 | Verifier denial-cluster ticker (policy denial clusters → synthetic correction prompt) | policy/verifier-cluster.ts, server.ts:7183 | absent | long-tail | evy-policy crate = native gate substrate |
| 4.18 | Upstream watchdog (pi-ai/pi-agent-core npm poll, 6h + auto-update) | upstream-check.ts, server.ts:7200 | absent | long-tail | **Contestable: npm upstreams are v3-stack — strong retire-with-v3 candidate** |
| 4.19 | Consciousness loop (config-gated planner: signals → rule-based plan → safe actions) | consciousness-loop/, server.ts:7213 | absent | long-tail | flagship autonomy behavior; evy-thinking crate is a different surface (planning sessions). **Operator call on port shape** |
| 4.20 | Idle-pane watchdog (typed-but-unsubmitted directive detection) | idle-pane-watchdog.ts, server.ts:7292 | absent (impl EXISTS in evy-watchdog crate, NOT armed — dormancy closet-logged at X2) | W6 | arm at boot = small W6 slice |
| 4.21 | Watchdog-prune sweep (stale watchdog registry rows) | watchdog-prune.ts | absent (impl exists in evy-watchdog, dormant) | W6 | same X2 closet entry |
| 4.22 | Watchdog heartbeat registry (touchWatchdog liveness + diag) | watchdogs.ts | native (WatchdogDiagRegistry + HeartbeatWatchdog) | done | |
| 4.23 | SSE broadcast hub (events fan-out) | server.ts broadcast() | native (/api/evy/events) | done | |
| 4.24 | Skill router (per-turn skill preload scoring) | skill-router.ts | native (evy-skills crate registry + router; v4 chat wires skill_view) | done | depth parity vs v3 scorer unverified — re-check in Evy-agency wave |
| 4.25 | Secrets resolution chain (env → secrets.json → backends/1Password) | secrets.ts, secrets-backends.ts | native (evy-secrets crate) | done | HTTP admin routes for it absent (1b #12–14) |
| 4.26 | Voice runtime (TTS client, voice.json watcher, cache, redaction) | voice-config.ts + master voice routes | native substrate (evy-voice crate) | long-tail | HTTP + tool surfaces unported (1b #54–57); W5 non-goal |
| 4.27 | Personality/profile hot-reload | personality.ts, profiles.ts | absent | long-tail | pairs with 1b #4, #47 |
| 4.28 | Cron-shaped scheduler (persisted operator-definable jobs) | — (v3 nearest: follow-ups) | native (evy-scheduler crate, jobs survive restart) | done | v4-new superset |

---

## Summary

### By status (all classes, 103 + 60 + 30 + 45 + 8 + 5 + 28 = 279 censused rows)

| Status | 1a routes | 1b routes | 2 tool modules | 3 CLI (verbs+bins+helpers) | 4 runtime | Total |
|---|---|---|---|---|---|---|
| native (done) | 40 | 17 | 0 | 3 | 9 | **69** |
| proxied | 63 | 17 | 0 | 50 | 0 | **130** |
| absent | 0 | 26 | 30 (all modules) | 0 | 19 | **75** |
| independent | 0 | 0 | 0 | 5 | 0 | **5** |

*(Row-level counts: dashboard natives = rows 1–9, 14–18, 26–27, 37, 41–46, 47, 52, 53, 65, 79–80, 83, 94–95, 100–103 incl. multi-method rows counted once. Post-S2: +4 native, −4 proxied vs the merged v1.)*

### By assigned wave (non-done rows)

| Wave | Rows | Headline contents |
|---|---|---|
| **W5** (CLOSED — S2 verified) | 0 remaining | 4 rows shipped native (memory tier1, bare /api/memory, cognee + memori thin forwards); 6 rows (/api/memory/{stats,recent,search,entries,{id}} both dialects) re-rowed to Evy-agency-or-later after the S2 tripwire (master-owned SQLite store) |
| **W6** | 10 (+1 note-level latent) | latent SSEs (/api/update/events, logs/stream, notifications/stream), dead /api/preferences restoration, vault browser family, logs reads, arm idle-pane + prune watchdogs, tier1 config-dir symlink + `$HOME` hardcode fix (carried as note on 1a #47) |
| **Evy-agency** | 40 | all 28 wired tool modules (99 tools), /api/teams/tools, inbox poll, follow-up ticker, background-runs, circuit breaker, + 6 master-memory-store routes (Evy-agency-or-later, ownership-blocked) |
| **long-tail** | ~100 | skills panel family, policy/audit family, providers/catalogs (pi-ai ⚠), auth flows, voice family, memory kernel + tier1 workflow + backfills, local-backend, personality/profile, upstreams ⚠, MCP server, master live-teams view, most CLI verbs |
| **W7-prep** | ~40 | install/update/deploy/service/uninstall machinery, settings keys/secrets, bare-path repoints (sessions/orchestration/watchdogs/notifications), /api/live, doctor/status/logs CLI, cheat/help |

### Top-5 biggest ABSENT surfaces by risk

1. **The entire Evy tool desk — 99 wired tools across 28 modules (Evy-agency).** v4 chat already shipped without it and the operator hit the wall live (can't answer usage/orch/spawn questions). Largest single parity gap; everything else routes around v3, this one defines what Evy *is*.
2. **The memory consciousness stack — kernel ticker, tier1 capture/approve/consolidate, cognee promotion, backfills (1b #25–35, 4.7–4.9).** Evy's learning loop. Retiring v3 without it silently freezes Tier 1/3→4 memory formation — no error, just amnesia accruing.
3. **The v3 management CLI — ~45 verbs the operator (and the dashboard's copy-paste snippets) use daily.** Today saved by the shim fall-through into the v3 repo tree; "retirement" that deletes or stops maintaining that tree breaks accounts/auth/teams/radar muscle memory. Needs an explicit keep-bash-vs-port ruling per family.
4. **MCP server on :8788 (1b #1, 4.14).** The only surface NOT behind the v4 proxy — external MCP clients die silently the moment the v3 master stops, with no proxy to soften it.
5. **Autonomy loops — consciousness loop, follow-up ticker, inbox poll, auto-compact (4.19, 4.10, 4.1, 4.13).** Without them v4 Evy is request/response only: no self-initiated actions, no scheduled follow-through, transcripts grow unbounded.

### Contestable wave assignments (flagged for orchestrator/operator)

- **Upstream watchdog + /api/upstreams family** (4.18, 1a #78): assigned long-tail, but pi-ai/pi-agent-core npm upstreams are v3-stack concepts — strong retire-with-v3 candidate. Cheapest correct answer may be deletion.
- **Vault browser family** (1a #48–51): assigned W6; arguably belongs to the W5 memory tab family but the locked W5 recon table omits it — did not expand S2's scope unilaterally.
- **Skills panel family** (1a #28–35): assigned long-tail; file-backed reads would also fit W6. Kept together as one family rather than splitting reads/writes.
- **MCP server** (1b #1): long-tail as "port", but the scope (which tools, which token story) is really an operator decision tied to Evy-agency.
- **Auto-nudge** (4.5): v3 already demoted it to broadcast-only; port-vs-retire is a judgment call.
- **Unwired tool modules** (preferences.ts, team-templates.ts) + **dead /api/preferences route** (1a #77): v3 ships these broken/disconnected today. Port-or-drop needs one explicit decision — restoring intended behavior in v4 would follow the W3 policy-presets precedent (sanctioned parity deviation).
- **web.ts (Brave/Firecrawl) vs evy-research (TinyFish)**: porting both search stacks vs consolidating on TinyFish is an Evy-agency design call.

### Standing operator checkpoints carried into W7 (from prior rulings, default flipped to "port — schedule it")

1. pi-ai catalog port effort (providers/catalogs family) — days of work, no Rust port exists.
2. Terminal real-PTY libc/openpty waiver (terminal family).
3. Telegram voice-note creds (telegram_send_voice + inbound voice).

---

## Amendments (post-S2, W5 memory-wave verification — w-memory worker, relayed by orchestrator)

1. **TRIPWIRE FIRED on `/api/memory/{stats,recent,search,entries/*}`** (1a #81, 1b #20–24): backing store is master's OWN SQLite at `~/.local/state/subctl/memory/evy.db` (components/evy/memory.ts:24,73) with daemon-side egress redaction (server.ts:4882–4886) — **NOT claude-mem** as the recon table assumed. Rows flipped to owner = v3 master runtime store, status = proxied, wave = **Evy-agency-or-later** (blocked on store ownership, not effort). Any future native port must reproduce the redaction layer.
2. **Shipped native in W5** (flipped to done): `/api/memory/tier1` GET/POST (1a #47), bare `/api/memory` GET (1a #52), `/api/cognee/*` (1a #79, sidecar :8745 / `SUBCTL_COGNEE_PORT`), `/api/memori/*` (1a #80, sidecar :8746 / `SUBCTL_MEMORI_PORT`).
3. **New latent (W6 candidate, noted on 1a #47 + Class-2 tier1-memory row):** v3 tier1-memory.ts writes `~/.config/subctl/master/`, which reaches the documented `evy/` dir only via a host-local symlink — fresh installs without the symlink silently lose dashboard tier1 edits. Also the dashboard tier1 block hardcodes `$HOME` and ignores `SUBCTL_CONFIG_DIR` (server.ts:4752), inconsistent with the rest of server.ts.
4. **Strangler note:** memory.js kernel + backfill panels still require v3+master up after this wave (expected strangler state) — noted on 1b #25 and #33.

Net effect on totals: native 65→69, proxied 134→130; W5 closed; Evy-agency 34→40.
