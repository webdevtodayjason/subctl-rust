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
| P3-C | slice 2: native handlers absorb the BFF UI contract (body dialect, fire-and-forget→events bus, shared current-session, New Chat reset) | IN-FLIGHT — same branch, append-only | p3a-chat-dialect | 2026-06-10T22:40Z | |

## Decision Log

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

