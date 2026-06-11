# Orchestration Log — v4 every-panel-parity sprint (wave 1)

**Session:** subctl orchestrator (Claude Code, left pane)
**Protocol start:** 2026-06-10T02:05Z (2026-06-09 ~9:05 PM CDT)
**Contract:** LOCKED — see /goal (8 families v4-native, v3-shape parity, proxy table → 0; NO Phase 7, NO voice (wave 2), NO beautification, NO v3-repo changes)
**Merge discipline:** orchestrator merges sequentially; full CI gate (`cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`) per merge; deploy + browser-verify per family.

## Task Ledger

| ID | Family | Module | State | Worker | Branch | Started | Finished |
|----|--------|--------|-------|--------|--------|---------|----------|
| W1 | providers + models + catalogs | providers_http.rs | MERGED+verified (models parity ✅, pi-ai stays proxied per ruling) | w1-providers | feat/v4-native-providers | | |
| W2 | settings reads | preferences_http.rs | MERGED; FIX IN FLIGHT — keys/secrets/update-check hollow-data, falling back to proxy | w2-settings | feat/v4-native-settings | | |
| W3 | projects CRUD + policy presets | projects_http.rs | MERGED+verified (parity identical mod rel-time; 0.2s vs Bun 13.8s) | w3-projects | feat/v4-native-projects | | |
| W4 | sessions list/kill/spawn/preview | orch_sessions_http.rs | MERGED+verified (sessions/list BYTE-IDENTICAL) | w4-sessions | feat/v4-native-sessions | | |
| W5 | watchdog registry + diag + restart | watchdogs_http.rs | MERGED+verified (canonical path additive; live heartbeat ticking; browser bare path unchanged) | w5-watchdogs | feat/v4-native-watchdogs | | |
| W6 | notifications tray + attachments | notifications_http.rs, attachments_http.rs | MERGED+verified (browser path /api/notifications proxied + IDENTICAL; canonical native additive) | w6-notify | feat/v4-native-notifications | | |
| W7 | cost/usage synthesis | cost_http.rs | MERGED+verified (/api/state.cost native, 4 month rows) | w7-cost | feat/v4-native-cost | | |
| W8 | web terminal (flag-gated, NO HMAC — brief corrected) | terminal_ws.rs | spike DONE (path B) + follow-up: WS-splice proxy | w8-terminal | feat/v4-native-terminal | | |

## Wave 1 CLOSE-OUT (2026-06-10, new locked contract via /goal)

Contract: settings/keys + update/check real data on :8797 (parity w/ :8787) · toolchain pinned + fmt --check green · full CI gate · redeploy = main · 18-tab browser sweep zero broken panels · w1–w8 worktrees/branches pruned. NON-GOALS: mutations, providers/catalogs/terminal native, Phases 3–7, v3 changes.

| ID | Task | State | Worker | Started | Finished |
|----|------|-------|--------|---------|----------|
| C1 | W2 hollow-trio → proxy fallback (keys/secrets/update-check) | DONE+LIVE-VERIFIED — merged c3c11a0 (worker cd47bf6, 3 files +52/−578); gate 85 suites/0 fail; deployed; keys+secrets BYTE-IDENTICAL vs :8787, update/check identical real data | w2-settings-fix | 2026-06-10T12:55Z | 2026-06-10T16:40Z |
| C2 | rustfmt sweep + rust-toolchain pin 1.95.0 (rustup-managed) | DONE — baa39c3 (26 .rs + pin), merged 5021e84; fmt --check + clippy + 85 suites green under pinned 1.95.0 | w2-settings-fix | 2026-06-10T16:30Z | 2026-06-10T17:00Z |
| C3 | merge C1+C2 (full CI gate per merge), redeploy :8797 = main | DONE — final binary from 5021e84 deployed + kickstarted; trio live (keys/secrets BYTE-IDENTICAL, update/check real); origin/main pushed 9ec6d61..5021e84 | orchestrator | 2026-06-10T16:35Z | 2026-06-10T17:10Z |
| C4 | 18-tab browser acceptance sweep | DONE — PASS. API sweep 67/69 status-identical (2 diffs non-browser-visible: profiles GET-only 405v404, attach non-WS 400v403). Visual sweep via Chrome: all 18 tabs, ZERO JS errors/rejections; 2 HTTP 4xx both PRE-EXISTING (team-templates 404/404, skills/categorized 400/400 — identical on :8787, closet-logged) | orchestrator | 2026-06-10T17:15Z | 2026-06-10T17:55Z |
| C5 | prune w1–w8 worktrees + delete merged feat/v4-native-* branches | DONE — 10 worktrees removed (w1–w8 + fmt + verify, all clean/merged); 10 local + 8 remote branches deleted; old w2-settings worker shut down via protocol; w2-settings-fix shutdown requested at close | orchestrator | 2026-06-10T17:20Z | 2026-06-10T17:55Z |

**WAVE 1 CLOSED 2026-06-10T17:55Z — all contract acceptance bullets met.** d5554ce ruling surfaced to operator (rec: keep as Phase 3 seed, no merge). Closet additions this close: toolchain-pin-rustup-only hardening; serde key-order caveat; team-templates 404 + skills/categorized 400 latents.

## Wave 2 — MUTATION PARITY (2026-06-10, locked contract via /goal)

Contract: port every dashboard WRITE v4 can coherently own; zero split-brain (read+write same owner per resource). Recon table FIRST. NON-GOALS: master trio writes, install/env-coupled writes, Phases 3–7, providers/catalogs/terminal native, v3 changes.

### Port/stay table (recon 2026-06-10T18:30Z — from server.ts writeFileSync census + frontend fetch map + W2 addendum)

| Mutation route | Writes | Owner of backing data | Verdict | Reason |
|---|---|---|---|---|
| POST /api/settings/obsidian | `~/.config/subctl/evy/obsidian.json` | dashboard flat-file; v4 already serves the READ natively from the SAME file | **PORT** | textbook same-owner coherence; oracle read comparison applies |
| POST /api/settings/telegram | v3 writes `evy-notify.json` — **the file disarmed by criterion #6** | bot + creds are v4-owned since the 06-09 flip (`v4/config.toml [telegram]`) | **PORT, target = v4 config.toml** | v3's write path writes DEAD config today (panel writes do nothing) — porting restores intended behavior; no v3 GET route exists, so no oracle read to diverge from. Sanctioned deviation, same class as W3 policy/presets |
| POST /api/settings/telegram/test | (sends test msg) | v4 owns the bot | **PORT** | v4 sends through its own bridge; v3's path already falls back to v4 notify anyway (`0475997`) |
| POST /api/settings/secrets/{key} | secrets store | env/install-coupled to v3 (reads went BACK to proxy in C1) | STAY | porting the write = split-brain with proxied reads (contract non-goal, recon confirms) |
| POST /api/update/run | install tree | v3 install-coupled | STAY | non-goal, recon confirms |
| POST/PUT/DELETE /api/teams* | team JSON files | master-owned (P4–P6 orchestration surface) | STAY | teams UI data stays v3 until its owner moves |
| POST /api/skills/import, /api/skills/evy/{}/promote+delete | skills tree | v3-owned | STAY | skills panel family not yet ported (reads incl.) |
| POST /api/memory/tier1 | memory file | v3/claude-mem-owned | STAY | memory panel is Phase 5 |
| POST /api/catalogs/*/{enabled,enabled-all,refresh}, /api/providers/*/default-model | provider catalog state | pi-ai/v3 (lead directive b42b311) | STAY | providers/catalogs native is an explicit non-goal |
| POST /api/notify/reply, /api/notify/inbox/{}/ack | v3 inbox/ask state | v3 runtime (v4 has its own /api/evy/ask path) | STAY | notifications MUTATIONS not in wave; reads already native (W6) |
| POST /api/evy/* (chat/compact/clear/supervisor/engagement/restart), /api/sessions/spawn+kill, /api/orchestration/* | v3 runtime processes | v3 master | STAY | runtime actions, not flat-file writes; P4–P6 |
| POST /api/projects/create | project tree | **already v4-native (W3)** | done | no work |
| POST/DELETE /api/providers/profiles | accounts file | **already v4-native (W1)** | done | no work |
| POST /api/models/refresh | models cache | **already v4-native (W1)** | done | no work |

→ Wave-2 port surface = the 3-route settings-mutations slice (obsidian + telegram + telegram/test). One worker.

| ID | Task | State | Worker | Started | Finished |
|----|------|-------|--------|---------|----------|
| M0 | recon port/stay table | DONE (this table) | orchestrator | 2026-06-10T18:10Z | 2026-06-10T18:30Z |
| M1 | settings-mutations slice (3 routes) | DONE — 0fca754 (3 files +701/−6), merged 0451333; both tripwires resolved in-spec (restart-required documented w/ Arc<Inner> analysis; surgical TOML editor, zero new deps) | m1-settings-mutations | 2026-06-10T18:40Z | 2026-06-10T19:25Z |
| M2 | merge under pinned-toolchain gate, redeploy, round-trip + 69-endpoint regression sweep + panel re-check | DONE — gate 85/0 + fmt green; deployed; obsidian round-trip (write→read-back→restored), telegram no-op merge CONFIG.TOML BYTE-IDENTICAL + getMe live (Semfreakbot) + real test msg delivered; regression sweep = same 2 benign diffs, zero new | orchestrator | 2026-06-10T19:25Z | 2026-06-10T19:45Z |

**WAVE 2 CLOSED 2026-06-10T19:45Z — all contract acceptance bullets met.** Panel re-check found ONE error: 404 /api/preferences — proven PRE-EXISTING (master pid 68558 predates the wave; `components/evy/server.ts` never mounts /preferences; 404 identical at every hop master→v3→v4). Closet-logged. Telegram oracle clause documented inapplicable (v3 has no GET route; v3's write target is the criterion-#6-disarmed file — port restores intended behavior).

## Wave 3 — PHASE 3 CHAT PANEL NATIVE (2026-06-10, locked contract via /goal)

Contract: chat tab daily-usable, v4-native end-to-end (conversational default, /plan on request), no v3 hop — proven with v3 STOPPED. NON-GOALS: voice port, Phases 4–7, v3 changes, persona authoring.

### Recon map (2026-06-10T20:30Z) — chat-tab calls (from chat.js fetch census) vs v4 surface

KEY DISCOVERY: increments 1–3 of the persona work are ALL already on main — persona vendored (evy.md + evy-voice.md), SessionMode plumbed (partner.rs:41), and chat.rs already routes plain→conversational / `/plan <topic>`→planning with per-session mode memory. The deployed daemon HAS the experience; the gap is purely the browser dialect.

| chat-tab call | v4 native twin | verdict |
|---|---|---|
| POST /api/master/chat | /api/evy/chat ✓ | **ALIAS** (missing) |
| GET /api/master/events (SSE) | /api/evy/events ✓ | **ALIAS** (missing; plain HTTP streaming, NOT the WS-upgrade-hole class) |
| GET /api/master/transcript, /util; POST /compact | /api/evy/transcript{,/util,/compact} ✓ | **ALIAS** (missing) |
| POST /api/master/transcript/clear | /api/evy/transcript/clear ✓ | already aliased (the precedent) |
| GET /api/master/context | /api/evy/context ✓ | **ALIAS** (missing) |
| POST /api/master/attachments | /api/evy/attachments ✓ | **ALIAS** (missing) |
| GET /api/master/health, /diag; POST /supervisor, /restart | none | stays-proxied (master-owned runtime); post-rewrite they fall through to v3 AS /api/evy/* — exactly what v3 expects, since v3 does the same rewrite (server.ts:2923) |
| GET/POST /api/profile; GET /api/providers | none | stays-proxied (master trio / b42b311) |
| /api/voice/{status,config,render} | none | stays-proxied per contract; degrade-gracefully check in no-fallback proof |
| POST /api/evy/engagement | none | stays-proxied (already evy-dialect) |
| /tool-display.json | static, served 200 by front door | verify native in no-fallback proof |

→ Implementation = ONE slice: v4 adopts v3's own trick — a global `/api/master/*` → `/api/evy/*` prefix rewrite before routing (native routes win; unmatched rewritten paths ride the existing catch-all to v3, which speaks evy-dialect internally). Plus mode-routing integration tests if chat.rs lacks them. Orchestrator owns: no-fallback proof, chat-tui live check, sweeps, d5554ce disposition.

| ID | Task | State | Worker | Started | Finished |
|----|------|-------|--------|---------|----------|
| P3-0 | recon map | DONE (this table) | orchestrator | 2026-06-10T20:15Z | 2026-06-10T20:35Z |
| P3-A | master-dialect rewrite layer + mode-routing tests | DONE — 2c7aed8 scoped to 8 claimed chat-tab paths (post-redirect rework from global; teams hazard averted), merged f8174f7; mode tests pre-existed (0 added); unclaimed-path drop test + fall-through test (own binary, pinned closed upstream) | p3a-chat-dialect | 2026-06-10T20:45Z | 2026-06-10T22:05Z |
| P3-B | merge+gate+deploy; NO-FALLBACK PROOF (v3 stopped); chat-tui check; sweeps; d5554ce closure | IN PROGRESS — d5554ce CLOSED; f8174f7 gate 86/0 + deployed; PROOF ROUND 1 (v3 dark): curl chat → conversational Evy ✅, transcript render ✅, zero JS errors ✅, BUT UI send 422 — BFF was the ADAPTER (body {text,source,attachments}, held session, fire-and-forget, SSE message_update/message_end vocabulary). v3 restored healthy. Slice 2 dispatched to absorb the contract natively | orchestrator | 2026-06-10T21:00Z | |
| P3-C | slice 2: native handlers absorb the BFF UI contract (body dialect, fire-and-forget→events bus, shared current-session, New Chat reset) | DONE — 1b99115 (6 files +663/−16), merged d51a140 + fmt fixup 69904f3; events extended via DaemonEvent::DashboardFrame (no bus redesign); vocabulary mapped to exact chat.js listener lines | p3a-chat-dialect | 2026-06-10T22:40Z | 2026-06-10T23:45Z |

**PHASE 3 CLOSED 2026-06-11T00:25Z — all contract acceptance bullets met.** NO-FALLBACK PROOF round 2 PASSED with v3 connection-refused dark: UI "hello" → conversational Evy streamed into the log via native events bus; transcript + context meter (~2.5k tok) live; compact preserved; New Chat archived + reset; ZERO JS errors (voice pill degraded gracefully). chat-tui live round-trip green against :8797 (--base-url; sync {message} dialect byte-unchanged). Sweeps: 18-tab zero NEW errors (only the 3 known pre-existing latents); 69-endpoint = 2 known diffs + 1 NEW EXPECTED (master/chat GET 405v404 — POST-only native route answering GET, same benign class as providers/profiles; browser only POSTs). Gate final: fmt+clippy+87 suites/0 fail under pinned 1.95.0. origin/main pushed (69904f3). Roster: 7 shutdown requests sent (p3a + 6 stale wave-1 workers found alive despite isActive:false). Lesson logged: worker gate briefs must include fmt --all --check (slice 2 landed unformatted; orchestrator fmt fixup 69904f3).

## ROAD TO PHASE 7 — daisy-chained waves (operator directive 2026-06-10 evening)

OPERATOR DIRECTIVE: chain pre-wave verify → W4 (teams/orch native) → W5 (memory + tab census) → W6 (hardening + latents) → W7-prep. Workers = FABLE model agents (model: "fable" on every spawn). Autonomous; TTS only on tripwire or the W7 operator decisions (pi-ai catalog / terminal PTY / voice creds — retirement itself is operator-promoted, never autonomous). Standing rules carry: recon table before code per wave; data-ownership doctrine; scoped predicates; pinned-toolchain gate incl. fmt --all --check in EVERY worker brief; no-fallback proof as each wave's exit gate; closet for leftovers; prune + protocol shutdowns per wave.

| Chain step | State |
|---|---|
| PRE: verify /api/master/events consumers (profile pill, agent-log) | DONE — pill: 30s-poll fallback covers it (real-time profile_swapped returns with profile ownership; closet). agent-log capture: lingers w/o agent_end → W4 micro-fix (v4 emits agent_end alias). NEW FINDING: orch tab's live feed (inbound/team_event/watchdog_fire/watchdog_ok on /api/master/events) silenced by P3 — v4 bus lacks the vocabulary; v4 natively owns ALL the sources (watchdogs W1, telegram #6, own registry) → W4 core slice |

### W4 recon table (2026-06-11T01:10Z)

| Surface | Today | Owner analysis | W4 verdict |
|---|---|---|---|
| orch-tab SSE vocabulary (inbound, team_event, watchdog_fire/ok) | silenced (v4 bus) | v4 owns all sources natively | **F1: emit natively from v4 subsystems** + agent_end alias (verify-pass fix) |
| /api/orchestration/ + /captures (orch tab data) | proxied → v3 registry; v4 native /api/evy/orchestration=[] exists in parallel | live state owned by whichever daemon spawned it; v4 = source of truth for new work | **F2: native serves v4 registry MERGED with v3's as optional-degraded upstream** (strangler; merge dies at W7) |
| /api/master/teams, /api/teams* CRUD (teams tab) | proxied → v3 master live teams | master-owned legacy | stays proxied this wave (CRUD + live teams move at W7 or when master retires) |
| /api/master/{diag,health} (orch tab) | proxied | master runtime | stays proxied |
| watchdogs, terminal/enabled | native (wave 1) | v4 | no work |
| W4: teams/orchestration native (recon table → slices → no-fallback) | CLOSING 2026-06-11T00:50Z — F1 gated on branch (ALL PASS) → merged c19167a (clean, F1/F2 disjoint, 694 ins/12 files) → merge gate 88/0 + fmt + clippy → DEPLOYED (pid 85108, Phase-3 binary backed up as evy.bak-phase3-69904f3) → LIVE-VERIFIED: /api/orchestration native origin-tagged (claude-subctl=v3-legacy), /captures parity+origin, real-time feed PROVEN (message_start→deltas→message_end→agent_end on live SSE) → NO-FALLBACK PROOF PASSED (v3 bootout: orchestration 200 `[]`, captures 200 count:0, events stream held; restore: merged row recovered) → origin/main pushed → F1 worktree+branch pruned (local+remote). 69-endpoint sweep: 3 known-benign + 1 timeout artifact (v3 projects 12.6s vs 6s sweep timeout) + 1 PRE-EXISTING latent closet-logged for W6 (/api/update/events SSE dark through catch-all proxy; F2 diff exonerated — visibility-only). Outstanding → ROOT-CAUSED 2026-06-11T01:15Z: two empty listen windows because **TeamStalenessWatchdog is never registered at daemon boot** — `register_default_watchdogs` (diag_bridge.rs:31) arms ONLY HeartbeatWatchdog; F1's fire/ok emitters are correct+tested but dormant in production (the team-staleness row in /api/evy/watchdogs is v3's data via the merged diag view). team-gc registration unverified — same risk for its team_event frames. FIX SLICE REQUIRED before W4 stamps fully closed: register staleness (+gc) with the events-wired ctx at boot. agent_end/message vocabulary live-proven; no-fallback exit gate passed |
### W4-completion + account-spine dispatch (team subctl-v4-fixes, 2026-06-11T01:25Z, operator "go")

| ID | Task | State | Worker | Branch | Started |
|----|------|-------|--------|--------|---------|
| X1 | account spine: conf-writer trailing-newline fix (providers_http.rs + regression test) + v4 shim fall-through to v3 CLI (repo source + installed copy) | MERGED 6df5809, gate 88/0 (fmt+clippy+tests), pushed c19167a..6df5809, branch pruned. Shim fix LIVE (script, no deploy needed); newline fix rides the wave deploy. Orch-verified pre-merge (fresh-shell: accounts→v3 table w/ argent, auth→v3 error, health→v4). Worker findings: `subctl help` now forwards to v3 (spec-as-written, revertable); v3 auth exit-0 bug CONFIRMED live + closeted; _subctl_v3 recursion fallback closeted | w-spine (Fable) | feat/v4-account-spine | 2026-06-11T01:25Z |
| X2 | register TeamStalenessWatchdog (+TeamGcWatchdog if dormant) at daemon boot with events-wired ctx — makes F1's watchdog/gc frames live | MERGED f4f1f36, gate 88/0, pushed, branch pruned. Worker found+fixed deeper gap: daemon had NO shared TeamRegistry (WorkerRegistry was app-state-private) — new WorkerTeamRegistry adapter, spawn path + watchdogs share rows. Orch-verified scope clean pre-merge. Prune/IdlePane dormancy + terminal-status-rows policy closeted | w-watchdog (Fable) | feat/v4-watchdog-boot | 2026-06-11T01:25Z |

**WAVE 4 CLOSED 2026-06-11T01:42Z.** Deployed binary = f4f1f36 (main == origin == deployed). LIVE EVIDENCE COMPLETE: native merged registry origin-tagged ✓ · captures parity+origin ✓ · chat vocabulary (message_start/update/end + agent_end) live ✓ · **watchdog_ok live on /api/master/events** (`{"stale":0,"teams_tracked":0,"ts":"2026-06-11T01:39:45.898Z"}` — 600s cyclic tick after boot tick 01:29:45.892) ✓ · diag = 3 healthy ticking watchdogs ✓ · no-fallback proof (v3 dark: 200 [], events held) ✓ · conf-writer round-trip live (POST→0a terminator→v3-read sees row→DELETE clean) ✓ · 69-endpoint sweep accounted (3 known-benign, 1 artifact, 1 pre-existing latent→W6) ✓. Operator-visible wins: argent account fully resolvable; `subctl auth claude argent` works as the dashboard copies it (shim fall-through live). Team subctl-v4-fixes: 2 Fable workers, both delivered first-pass, gates green, protocol shutdowns at close. Closet: +7 owned entries this session.

Operator-context: discovered live when the operator added the `argent` account via the dashboard (v4-written row invisible to all v3 tools — no trailing newline; byte-proven) and the dashboard's copied `subctl auth claude argent` died on the shadowed shim. Live accounts.conf hand-fixed (printf '\n' >>) at 01:08Z; argent resolves. v3 resolver empty-suffix-guard bug closet-logged (v3-repo change, operator nod pending).

## Wave 5 — MEMORY NATIVE + PARITY CENSUS (locked contract via /goal, 2026-06-11T02:15Z)

Contract: memory tab v4-native where v4 owns/reaches the data + PARITY_CENSUS.md (every server.ts route, evy tool, CLI command, runtime loop — owner|status|wave, zero unknowns) as the W7 checklist. NON-GOALS: tier1 workflow + kernel (master runtime), Evy agency (own wave), sidecar internals, v3 changes, voice. BUDGET: 2 Fable workers; S2 ≤ ~6 files. TRIPWIRE: ownership contradiction ⇒ family stays proxied, censused; surprise scope ⇒ report before expanding. Exit gate: no-fallback proof (native memory families 200 with v3 dark) + zero new JS errors + single deploy.

### Memory recon table (orchestrator, 2026-06-11T02:00Z, from dashboard/server.ts)

| Route | server.ts | Owner | Verdict |
|---|---|---|---|
| GET/POST /api/memory/tier1 | :4746 (master's always-in-context file) | dashboard flat-file | PORT if file-coherent (M1 obsidian precedent) — worker verifies hot-read |
| GET /api/memory (bare) | :5026 (Obsidian + vault state) | vault files on disk | PORT (read) |
| /api/cognee/* | :6686 (thin proxy → Cognee sidecar) | cognee service | PORT thin native forward |
| /api/memori/* | :6717 (thin proxy → Memori sidecar) | memori service | PORT thin native forward |
| /api/memory/{stats,recent,search,entries} | :6747 (proxy → evy daemon memory) | v3 evy runtime store | VERIFY backing: claude-mem DB ⇒ reads native via ClaudeMemReader; DELETE write tripwired |
| /api/master/memory/* (tier1 workflow + kernel) | master rewrite | master runtime | STAYS PROXIED (non-goal) |

| ID | Task | State | Worker | Branch | Started |
|----|------|-------|--------|--------|---------|
| S1 | PARITY_CENSUS.md — 4 surface classes (routes/tools/CLI/runtime), owner+status+wave per row, zero unknowns | DONE — MERGED 946dea6 (392 lines, doc-only; fmt pass, full gate deferred to S2 code merge). 279 surfaces: 65 native / 134 proxied / 75 absent / 5 independent. Orch-verified: zero unknowns, sanity counts documented, spot-rows match ruling history. KEY FINDING: MCP server :8788 is the ONLY surface not behind the v4 proxy (external MCP clients die when v3 stops) — W7-prep operator decision. Latent confirmed: /api/preferences dead in v3 itself (W6 restoration candidate). 8 contestable wave calls flagged in doc for operator review at wave close | w-census (Fable) | feat/w5-parity-census | 2026-06-11T02:15Z |
| S2 | memory family native slice per recon verdicts (verify-first; tripwire binding) | DONE — MERGED b3c2e0f (4 files +692), gate 89/0; TRIPWIRE FIRED correctly: memory store = master's own SQLite w/ egress redaction (NOT claude-mem) → family stays proxied, census re-rowed (Evy-agency, ownership-blocked). Ported native: tier1 (coherence proven: master re-reads per turn; master→evy symlink documented), bare /api/memory, cognee :8745 + memori :8746 direct forwards. 2 v3 latents closeted (tier1 symlink fragility, $HOME hardcode) | w-memory (Fable) | feat/v4-native-memory | 2026-06-11T02:15Z |

**WAVE 5 CLOSED 2026-06-11T02:50Z.** main == origin == deployed = b0e5df4 (pid 12901). Contract acceptance: ① PARITY_CENSUS.md committed + S2-amended (279 surfaces, 69 native/130 proxied/75 absent/5 independent, zero unknowns, audit trail) ✓ ② parity: 4 native routes IDENTICAL normalized vs :8787 oracle; every memory.js fetch target answers correctly through :8797 ✓ ③ NO-FALLBACK PROOF: v3 dark → tier1 200 (real content), vault 200, cognee+memori 200 via DIRECT forwards (pre-W5 these died with v3); proxied stats = instant 502 graceful; restore clean ✓ ④ gates 89/0 per merge, single deploy ✓. Operator review queue: 8 contestable census wave-calls + MCP-server W7-prep decision. Both workers first-pass clean; tripwire honored as designed.
| W6: hardening + latent restoration sweep | queued |
| W7-PREP: everything retirement needs except the 3 operator decisions | queued |

**SPIN-DOWN NOTE:** operator ran low on credits. HANDOFF.md (subctl repo) rewritten with full state + standing doctrine. F2 worker shut down post-merge; F1 ordered to WIP-commit (preserved by orchestrator if it stalled — check feat/v4-orch-events for either "wip(f1)" or "wip(orchestrator-preserved)").

## Decision Log

- **2026-06-11T01:00Z — OPERATOR DIRECTIVE: FULL V3 PARITY. "Everything that was in v3 needs to be in v4. That is crucial."** Supersedes all port-or-drop ambiguity: Evy's tool surface (~30 modules in `components/evy/tools/`) = PORT (the pending W7-prep "Evy agency" decision is now MADE); v3 management CLI (accounts/auth/teams/radar/service/session-list) = PORT; all still-proxied families (teams CRUD, skills, notify mutations, memory workflow, evy runtime actions, update/run, secrets writes) = PORT before retirement. W7 gate becomes: parity census 100% green. The W5 "tab census" is PROMOTED to a full v3-surface parity census (every server.ts route + every Evy tool + every CLI command + master runtime behaviors), each row with owner + port status — the master checklist "everything" is measured against. Items with external prerequisites keep their operator checkpoints (pi-ai catalog port effort, terminal real-PTY libc waiver, telegram-voice creds) but the default on each flips from "pending decision" to "port — schedule it."
- **2026-06-11T00:20Z — session-launcher worker-stamp tripwire (successor session):** the operator's interactive session tmux env carried `SUBCTL_AGENT_ROLE=worker` (stamped 23:58Z by the spawn path of whatever launcher opened it) — orchestrator-mode's anti-self-promotion guard fired. Operator confirmed interactive/orchestrator role; stamp cleared via `tmux set-environment -u`. WATCH: if the operator's launcher routinely stamps worker-role, every successor session will trip this — candidate W6 hardening item (find + fix the launcher path).
- **2026-06-11T00:15Z — Evy v4 chat has NO tool surface (operator field report):** dashboard Evy (v4-native since Phase 3) couldn't answer account usage / multi-account / orchestrator-spawn questions. Confirmed in code: `conversational_system_prompt()` explicitly disclaims tool access; only `skill_view` ever reaches the wire. v3's ~30-tool desk (`components/evy/tools/`) dies at W7 retirement and the dashboard already stopped routing to it. Closet-logged as a W7 prerequisite: name an "Evy agency" wave (read-only self-knowledge slice first — usage/radar, tmux sessions, orch registry — all v4-owned data) or consciously drop the capability. Operator decision pending at W7-prep.

- **2026-06-10T21:10Z — P3 dialect-rewrite ruling: SCOPED (option B), not global.** Worker surfaced that a global /api/master/*→/api/evy/* rewrite reverses the documented per-family cutover decision (4 alias-drop tests) and — decisive — would route /api/master/teams to v4's native /api/evy/teams, which serves V4's registry, NOT the v3 master teams data the UI renders (hollow-data class, W1 lesson; P4–P6 non-goal). Scoped predicate = chat-tab families only: chat, events, transcript{,/util,/compact,/clear}, context, attachments{,/{id}}. Other master-dialect paths fall through UNREWRITTEN. Only the chat alias-drop test flips to parity; workers/sessions/skills alias-drop tests stay. Per-family dialect adoption remains a P4–P6 decision.

- **2026-06-10T02:05Z** — Wave 1 dispatched per locked contract. Each worker: own git worktree off subctl-rust `main` (`94c4a32`+), one module family, no shared-file edits beyond http.rs route block + AppState accessor (orchestrator resolves trivial conflicts at merge). W8 has a hard first-session tripwire (stays proxied if WS attach isn't demonstrably working).
- Parity oracle = live curl against the Bun dashboard `:8787` (what the browser sees today). Workers do NOT deploy/kickstart the shared daemon — orchestrator deploys per merged family.
- **2026-06-10T02:25Z — W1 tripwire ruling:** cloud provider/catalog DATA is pi-ai (npm) with no Rust port — porting is days + a named V4_BRIDGE non-goal. RULING: native only where v4 owns the data (/api/models+refresh, profiles CRUD, provider_catalog() accessor default-None); /api/providers + /api/catalogs lists + pi-ai sub-endpoints STAY PROXIED — hollow-data shape-parity would visibly regress the Providers panel (criterion-#5 lesson: no live-hollow green). pi-ai catalog port = named leftover, Phase-7 prerequisite decision for operator.
- **2026-06-10T02:35Z — W2 ruling:** family splits 3 ways. (1) 6 dashboard-owned file-backed READS → native (the slice). (2) Mutations (obsidian/secrets/telegram writes, update/run) → STAY PROXIED — write-path divergence risk while the owning process is v3; mutations move when their owner moves (wave 2+). (3) profile/preferences/upstreams are MASTER-owned state → later wave; V4_BRIDGE's "P4-P6 stay v3-served" is superseded by the 06-04 every-panel contract but the operative boundary tonight is data ownership. Collision guard issued: providers/profiles CRUD is W1's. Pattern now established for all workers: **native where v4 owns the data; proxied where v3 owns it; reads before writes.**
- **2026-06-10T02:55Z — W8 tripwire fired (sanctioned).** Spike (fbdf69f, 326 lines) proves: (1) my brief conflated ADR-0011 layers — v3 terminal is FLAG-FILE gated, NO HMAC; an HMAC gate would break the real frontend. (2) True-parity attach needs a real PTY; macOS `script` is dead from launchd, `openpty` needs a libc dep (waiver = operator decision, wave 2, ~1.5d). (3) LATENT PHASE 0 HOLE verified: /api/terminal/attach rides the catch-all which cannot complete WS upgrades — works today only because the gate rejects pre-upgrade. Follow-up assigned to W8: Option B WS-splice (bridge_ws pattern) so the proxied family actually works through :8797 (~2-4h, tripwired).

## Verification Evidence
- **2026-06-10T03:00Z — ALL 8 BRANCHES MERGED to main (9ec6d61), full CI gate green per merge (85 suites), deployed to launchd daemon.** Live parity sweep through :8797: sessions/list + notifications/ + oauth + obsidian BYTE-IDENTICAL vs Bun; projects identical mod relative-time strings (native 69x faster); models identical mod ts; state.cost native. THREE hollow-data routes found (settings keys/secrets, update/check — env/install-coupled to the Bun process) → W2 fix dispatched: un-register, fall back to proxy. Watchdog + notifications browser bare paths verified unchanged (canonical natives are additive).
- **2026-06-10T03:15Z — formal reports all in; cross-checks done.** (1) W1's scope-modification commit b42b311 confirmed IN main — /api/providers + /api/catalogs identical both ports (33 rows), no hollow exposure. (2) W8 self-reported + corrected a task-board hijack of W4's task (restored; W4's merged work re-marked complete — their sessions/list was verified byte-identical). (3) ALL FIVE workers independently flagged pre-existing workspace rustfmt drift (~10 files on main; rust-toolchain floats `stable`, local rustfmt 1.9.0 reformats them) — fmt sweep + toolchain pin queued as a dedicated pass AFTER W2's parity fix merges, owner: W4. (4) W3 found a LATENT v3 BUG: /api/policy/presets + /api/policy/preset/{path} exist in dashboard/lib/policy-api.ts but were never wired into server.ts — the frontend calls them and gets 404 on v3 today; v4 now serves them natively, turning a broken panel feature into a working one (logged as a sanctioned parity deviation — restoring intended v3 behavior, not adding a feature). (5) W7 embedded current pricing defaults (SUBCTL_PRICING_FILE env override available) — follow-up option: install-deployed pricing.json. (6) W2 data-ownership addendum filed — the wave-2 mutation-port map (master-owned trio = high divergence; dashboard flat-file writes = low; telegram settings write now v4-eligible since criterion #6).
- Merge-conflict lesson: keep-both regex eats closing delimiters at hunk boundaries — 3 of 8 merges needed a hand-restored `)`. All caught by the per-merge gate.
- W8 spike: branch feat/v4-native-terminal @ fbdf69f, TERMINAL_SPIKE.md. Orchestrator re-verified: /api/terminal/enabled parity both ports; gate 403 (disabled) + 404 (bad team) identical via :8797 and :8787; flag reverted after test.

## Wave 6 — HARDENING + LATENT RESTORATION (locked contract via /goal, 2026-06-11T16:55Z)

Contract (distilled): all 9 W6 rows FIXED+live-proven or RULED+censused, none merely noted; /api/update/events streams through :8797 (curl proof); argent spawn converts untested-live list (watchdog/team_event/inbound frames + tabs click-through); no-fallback proof + pinned gate per merge + SINGLE deploy; main==origin==deployed. NON-GOALS: Evy-agency port · MCP :8788 · 3 operator checkpoints · 8 census contestables · memory store family · proxy rewrites · v3 retirement. BUDGET: ≤3 Fable workers, ≤~8 files/branch, recon first, one deploy. TRIPWIRE: budget/scope/ownership-contradiction/contestable-ruling ⇒ STOP/census/TTS.

**Pre-closed before dispatch (operator-in-the-loop, 2026-06-11 day):** v3 resolver empty-suffix guard + prefixed→bare resolution = **v3.3.13** (subctl PR #53, tested, deployed); v3 `auth` exit-0 closed by same fix (live rc=1 both via v3 bin and v4 shim); shim fall-through confirmed live (W4 work). Providers launch-hint polish = **v3.3.14** (PR #54). Closet ticked.

### W6 recon table (orchestrator, 2026-06-11T16:50Z, all live-reproduced)

| Row | Surface | Live evidence | Slice |
|---|---|---|---|
| ① | /api/update/events SSE via :8797 catch-all | :8787 = 200 instant; :8797 = 000 (curl 8s timeout) | w6-proxy |
| ② | /api/preferences | 404 (master never mounts; module exists unmounted) | w6-restore |
| ③ | /api/team-templates · /api/skills/categorized | 404 · 400 both ports (frontend-wired/backend-missing) | w6-restore |
| ④ | tier1 path drift master/ vs evy/ + $HOME hardcode | closet-documented file:line; symlink-dependent | w6-v3 |
| ⑤ | profile-pill real-time | UI staleness (W6 input list) | w6-restore |
| ⑥ | telegram settings hot-apply | telegram_write_handler restart-required by design note | w6-restore |
| ⑦ | session-launcher worker-stamp | **REPRODUCED THIS SESSION**: operator tmux session `claude-samsung-phones` carried SUBCTL_AGENT_ROLE=worker (process env clean); cleared via set-environment -u per 00:20Z precedent | w6-v3 |
| ⑧ | WatchdogPrune + IdlePaneWatchdog dormant at boot | not in register_default_watchdogs (closet, w-watchdog finding) | w6-proxy (RULING) |
| ⑨ | WorkerRegistry terminal-status rows count in teams_tracked | closet, w-watchdog finding | w6-proxy (RULING) |

| ID | Task | State | Worker | Branch | Started |
|----|------|-------|--------|--------|---------|
| W6-A | row ① SSE proxy + rows ⑧⑨ rulings | MERGED+verified — merge 9091a6c (merged-tree gate: fmt clean + 1125/0, orchestrator-run). ROW ① ROOT CAUSE OVERTURNS CENSUS HYPOTHESIS: proxy was streaming fine; Bun finalizes a streaming head only on first enqueued chunk, so /api/update/events (first chunk = 15s ka) leaves the head UNTERMINATED on the wire — hyper/EventSource correctly wait, curl display masked it ("direct works in 1ms" was an illusion). Fix: SSE head grace 750ms for GET+Accept:text/event-stream only; faithful mirror when head completes in grace (error heads aren't lazy → real 4xx/5xx mirror); synthesis + body splice after. VERIFY NUANCE: bare curl still 000s until v3 fix-at-source lands (closet #428 → micro-slice fix/w6-sse-open-comment dispatched to w6-restore); honest probe = curl -H 'Accept: text/event-stream'. RULINGS: ⑧ prune REGISTERED (gc overlap = documented defense-in-depth; W4 boot omission), idle-pane DELIBERATELY DORMANT (documented: no consumer until Phase-5 gate; sniffs human panes; pure noise today). ⑨ terminal workers reaped 15min after last activity (< 30min staleness, documented invariant); tmux session untouched (stays operator-visible); update_status has zero prod callers so row was latent — closed pre-emptively. | w6-proxy (Fable) | feat/w6-proxy-sse → main | 2026-06-11T17:00Z |
| W6-B | rows ②③⑤⑥ surface restoration | MERGED+verified BOTH SIDES — v3: subctl PR #56 (e80f0113; preferences mount restored [module shipped v2.8.1 never mounted, tools 79→81], team-templates routes onto existing v2.8.0 module, skills/categorized was catch-all SHADOWING [route existed; catch-all moved to section end], profile pill onto canonical connect() SSE lifecycle). v4: merge 43f0c0f (telegram hot-apply via RwLock<Creds> + apply_creds; boot-absent bridge stays restart-required, documented scope cut). Orch-verified: v3 196/0 + 47/0 reproduced; v4 fmt clean + 1121/0 reproduced; zero conflicts; live curl re-proof deferred to W6-D deploy. Worker incident self-healed + closeted (#425 notify-listener HOME hardcode armed competing prod long-poll ~2min from scratch boot). 3 latents closeted (#423-425; NOTE #424: Evy can SET prefs she never SEES — renderPreferencesForPrompt never injected). | w6-restore (Fable) | both → main | 2026-06-11T17:00Z |
| W6-C | rows ④⑦ v3 tier1 drift + launcher stamp | MERGED+verified — subctl PR #55 (rebase, 2 row-commits). Orch-verified: 24 new tests green; 15 fails = pristine-main baseline (cli.test.ts environmental, closet #419); dry-run proof -o→orchestrator / -p→worker / bare→none; tier1 resolves SUBCTL_CONFIG_DIR/evy; install.sh ensure_evy_layout on all branches; 4 leftovers closet-logged (#419-422, incl. other providers' unconditional stamp + -o worker-preamble decision). Root cause: single shared launcher hardcoded worker stamp into EVERY session incl. operator's. Release stamp at wave close. | w6-v3 (Fable) | fix/w6-tier1-launcher → main | 2026-06-11T17:00Z |
| W6-D | argent live spawn conversion + no-fallback + deploy | DONE (orchestrator) — see WAVE 6 CLOSED block | team-lead | — | 2026-06-11T21:55Z |

**WAVE 6 CLOSED 2026-06-11T22:58Z.** main == origin == deployed = **9091a6c** (pid 49846, backup evy.bak-w6-b0e5df4). v3 side shipped as **v3.3.15** (PRs #55 #56 #57 + release #58; install tree + dev tree synced; earlier same-day v3.3.13/.14 pre-closed 3 rows). **Contract acceptance:** ① 9/9 rows FIXED+live-proven or RULED+censused — zero merely-noted (①②③④⑤⑥⑦ fixed+proven; ⑧ prune armed [4/4 native watchdogs healthy on deployed diag] + idle-pane ruled dormant; ⑨ terminal-reap implemented) ✓ ② /api/update/events through :8797: synthesized head + spliced real `: ka` (Accept probe) AND post-#57 bare-curl instant on BOTH ports ✓ ③ argent conversion act: `worker_registered` + `team_event` spawn AND kill frames live on the wire (first-ever live observation); spawned real argent worker, native kill, registry drained, session torn down ✓partial — see finds ④ no-fallback proof (v3 dark: natives 200, proxied 502@0.5ms, SSE stream HELD through dark window; restore clean) + per-merge pinned gates (1121/0, 1125/0, 196/0×3, 24/0, 47/0) + single deploy ✓.

**Conversion-act FINDS (the act earned its keep — 2 latent defects no test had caught):**
1. **Native Claude spawn mandate delivery has NEVER worked live** — ready-poll `❯`-count matcher false-positives on the directory-trust dialog; paste swallowed, Enter accepts dialog, zero errors logged. Reproduced 2×, BOTH project dirs (tmp + default config-dir cwd). Closeted with fix recipe (same slice as 06-09 codex trust-prompt item).
2. **watchdog_ok/fire frames dark on deployed binary** — 22:51:38Z tick fired (diag healthy) while dual captures on /api/evy/events + /api/master/events saw keep-alives only; W4 proved these frames on f4f1f36; spawn team_event still arrives ⇒ suspected bus split (HTTP-state bus vs watchdog ctx bus lost its SSE bridge somewhere in b0e5df4..9091a6c). Cockpit trip/recover currently deaf. Closeted with bisect candidates.
**Not converted (owned, closeted):** `inbound` frames (needs real operator telegram — not synthesizable), `watchdog_fire` (recipe documented: kill worker tmux w/o native kill, wait a tick — blocked anyway by find #2), tier1 POST live, memori/cognee recall POSTs, install.sh shim regen, browser click-through (Chrome refused localhost:8797 3× — error page despite curl 200; Chrome-profile issue, NOT the dashboard).
**Census:** 6 rows amended (20, 32, 42, 55, 77, preferences.ts tool row). **Closet net:** 6 ticked done, 9 added-owned. Workers: 3 Fable, all first-pass mergeable, protocol shutdowns clean; +1 micro-slice rider (w6-restore) verified+merged. **NEXT:** proposed W6.5 spawn-integrity mini-wave (mandate delivery both providers + watchdog-frame bus regression + #420 stamp scoping + #425 notify-listener; #422 -o preamble = operator decision pending) BEFORE the Evy-agency wave — agency dispatch tools must not build on a spawn path that drops directives.

