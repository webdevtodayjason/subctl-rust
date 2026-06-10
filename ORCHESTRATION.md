# Orchestration Log — v4 every-panel-parity sprint (wave 1)

**Session:** subctl orchestrator (Claude Code, left pane)
**Protocol start:** 2026-06-10T02:05Z (2026-06-09 ~9:05 PM CDT)
**Contract:** LOCKED — see /goal (8 families v4-native, v3-shape parity, proxy table → 0; NO Phase 7, NO voice (wave 2), NO beautification, NO v3-repo changes)
**Merge discipline:** orchestrator merges sequentially; full CI gate (`cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`) per merge; deploy + browser-verify per family.

## Task Ledger

| ID | Family | Module | State | Worker | Branch | Started | Finished |
|----|--------|--------|-------|--------|--------|---------|----------|
| W1 | providers + models + catalogs | providers_http.rs | in-flight 02:10Z | w1-providers | feat/v4-native-providers | | |
| W2 | settings/preferences/profile/auth/update/upstreams | preferences_http.rs | in-flight 02:10Z | w2-settings | feat/v4-native-settings | | |
| W3 | projects CRUD + policy presets | projects_http.rs | in-flight 02:10Z | w3-projects | feat/v4-native-projects | | |
| W4 | sessions list/kill/spawn/preview | orch_sessions_http.rs | in-flight 02:10Z | w4-sessions | feat/v4-native-sessions | | |
| W5 | watchdog registry + diag + restart | watchdogs_http.rs | in-flight 02:10Z | w5-watchdogs | feat/v4-native-watchdogs | | |
| W6 | notifications tray + attachments | notifications_http.rs, attachments_http.rs | in-flight 02:10Z | w6-notify | feat/v4-native-notifications | | |
| W7 | cost/usage synthesis | cost_http.rs | in-flight 02:10Z | w7-cost | feat/v4-native-cost | | |
| W8 | web terminal (flag-gated, NO HMAC — brief corrected) | terminal_ws.rs | spike DONE (path B) + follow-up: WS-splice proxy | w8-terminal | feat/v4-native-terminal | | |

## Decision Log

- **2026-06-10T02:05Z** — Wave 1 dispatched per locked contract. Each worker: own git worktree off subctl-rust `main` (`94c4a32`+), one module family, no shared-file edits beyond http.rs route block + AppState accessor (orchestrator resolves trivial conflicts at merge). W8 has a hard first-session tripwire (stays proxied if WS attach isn't demonstrably working).
- Parity oracle = live curl against the Bun dashboard `:8787` (what the browser sees today). Workers do NOT deploy/kickstart the shared daemon — orchestrator deploys per merged family.
- **2026-06-10T02:25Z — W1 tripwire ruling:** cloud provider/catalog DATA is pi-ai (npm) with no Rust port — porting is days + a named V4_BRIDGE non-goal. RULING: native only where v4 owns the data (/api/models+refresh, profiles CRUD, provider_catalog() accessor default-None); /api/providers + /api/catalogs lists + pi-ai sub-endpoints STAY PROXIED — hollow-data shape-parity would visibly regress the Providers panel (criterion-#5 lesson: no live-hollow green). pi-ai catalog port = named leftover, Phase-7 prerequisite decision for operator.
- **2026-06-10T02:35Z — W2 ruling:** family splits 3 ways. (1) 6 dashboard-owned file-backed READS → native (the slice). (2) Mutations (obsidian/secrets/telegram writes, update/run) → STAY PROXIED — write-path divergence risk while the owning process is v3; mutations move when their owner moves (wave 2+). (3) profile/preferences/upstreams are MASTER-owned state → later wave; V4_BRIDGE's "P4-P6 stay v3-served" is superseded by the 06-04 every-panel contract but the operative boundary tonight is data ownership. Collision guard issued: providers/profiles CRUD is W1's. Pattern now established for all workers: **native where v4 owns the data; proxied where v3 owns it; reads before writes.**
- **2026-06-10T02:55Z — W8 tripwire fired (sanctioned).** Spike (fbdf69f, 326 lines) proves: (1) my brief conflated ADR-0011 layers — v3 terminal is FLAG-FILE gated, NO HMAC; an HMAC gate would break the real frontend. (2) True-parity attach needs a real PTY; macOS `script` is dead from launchd, `openpty` needs a libc dep (waiver = operator decision, wave 2, ~1.5d). (3) LATENT PHASE 0 HOLE verified: /api/terminal/attach rides the catch-all which cannot complete WS upgrades — works today only because the gate rejects pre-upgrade. Follow-up assigned to W8: Option B WS-splice (bridge_ws pattern) so the proxied family actually works through :8797 (~2-4h, tripwired).

## Verification Evidence
- W8 spike: branch feat/v4-native-terminal @ fbdf69f, TERMINAL_SPIKE.md. Orchestrator re-verified: /api/terminal/enabled parity both ports; gate 403 (disabled) + 404 (bad team) identical via :8797 and :8787; flag reverted after test.

