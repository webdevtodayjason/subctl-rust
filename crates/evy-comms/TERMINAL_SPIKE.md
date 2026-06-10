# Web Terminal — v4-native port spike (W8)

**Outcome: DONE-WHEN path B (tripwire).** The core attach (WS ↔ tmux pane
stream) is **not** demonstrably working as reliable v3-shape parity within the
locked contract constraints. The terminal family **stays proxied** to the v3 Bun
dashboard. This is a contract-sanctioned outcome, not a failure.

Branch: `feat/v4-native-terminal`. No routing changed — `/api/terminal/*` still
falls through `/api/{*rest}` → `reverse_proxy_handler` exactly as before.

---

## 1. What v3 actually does (the parity oracle)

Source of truth: `subctl/dashboard/terminal.ts` + `dashboard/server.ts`
(lines 2965-2999) + `dashboard/public/terminal.js`.

**Routes (browser-facing):**

| Route | Method | v3 behaviour |
|---|---|---|
| `/api/terminal/enabled` | GET | always `200 {ok:true, enabled, flag_path}` |
| `/api/terminal/teams` | GET | `200 {ok:true, count, teams:[{name,session,attached}]}`; `403 {ok:false, error:"terminal disabled"}` when off |
| `/api/terminal/attach?team=&cols=&rows=` | WS | real `tmux attach`; pre-upgrade gate returns `{ok:false, error}` with 403/400/404 |

Note the path is `/api/terminal/*`, **not** `/api/evy/terminal/*`. The browser
client (`terminal.js:74`) opens `ws://<host>/api/terminal/attach?team=…`.

**Browser wire protocol (must match exactly):**
- client → server: JSON **text** frames only —
  `{"type":"data","b64":"<base64 stdin bytes>"}` and
  `{"type":"resize","cols":N,"rows":N}`
- server → client: **binary** frames of raw pty bytes (xterm.js renders natively;
  server never sends JSON to the client).

**Gate (the entire security model — there is NO HMAC):**
1. Flag file `~/.config/subctl/terminal.enabled` exists (presence = on, default OFF).
2. Host-header check (DNS-rebind defence) when bound to localhost.
3. `team` param matches `^[A-Za-z0-9._-]{1,128}$` **and** exists as a live tmux
   session (`tmux list-sessions`).

**Mechanism:** v3 spawns a Node sidecar (`dashboard/lib/pty-helper.cjs`) that runs
`tmux attach -t <session>` **inside a node-pty**. The pty is the whole point —
`tmux attach` requires a controlling TTY. The browser↔helper framing
(`FRAME_DATA/RESIZE/CLOSE/EXIT/ERROR`) is an internal detail; only the
JSON-in/binary-out browser protocol above is the parity contract.

---

## 2. ⚠️ The brief's "HMAC-gated terminal" is a Layer-1/Layer-2 conflation

The dispatch said *"HMAC-gated WebSocket terminal attach"* + *"per-team HMAC
secret `~/.local/state/subctl/teams/<id>/hmac.secret`"*. **The v3 web terminal
uses no HMAC.** Both features live under ADR 0011, but they are different layers:

- **ADR 0011 Layer 1** — HMAC-authenticated supervisor **directives**. The
  per-team `hmac.secret` is generated in `providers/claude/teams.sh:380-414` and
  consumed by `components/evy/trust-marker.ts` / `handoff-directive.ts` to sign
  the `[subctl-master directive · … · hmac:<16hex>]` markers injected into worker
  panes. **Nothing to do with the terminal.**
- **ADR 0011 Layer 2** — the web terminal escape hatch. Gated by the flag file
  only. `terminal.ts:20-23` is explicit: *"No new auth surface… adding one here
  would be inventing new policy. The flag file is the opt-in."*

**Implication for parity:** adding HMAC to the attach would (a) reject the real
frontend, which sends no HMAC token → terminal breaks, and (b) violate
"v3-shape parity, no gold-plating." **The native port must gate on the flag file,
not HMAC.** Whoever picks this up should confirm this reading with the operator
before writing any HMAC code.

---

## 3. What works (proven by spike)

- **Output streaming via `tmux pipe-pane`** — *works.*
  `tmux pipe-pane -o -t <session> "cat >> <sink>"` streams the raw, escape-laden
  pane byte stream (verified: SGR colours, cursor moves, `?2004h` bracketed-paste
  all captured) — exactly the bytes xterm.js consumes. Seed the initial screen
  with `tmux capture-pane -e -p -t <session>`.
- **Gate / `enabled` / `teams` logic** — *trivial, pure, no deps.* Direct port of
  the `terminal.ts` functions to Rust (flag-file `existsSync` → `Path::exists`,
  team-name regex, `tmux list-sessions -F` lister). Fully unit-testable with an
  injected lister and a tmp flag dir. Ready-to-use code in the Appendix.
- **`tmux send-keys -H <hex>`** — *works for short injections.* `-H 41 42 43`
  reliably typed `ABC`; control bytes inject too. **But see §4 for the catch.**
- **WS upgrade + pre-upgrade JSON rejection** — axum `WebSocketUpgrade` lets a
  handler return a plain `(StatusCode, Json)` response before upgrading, which
  reproduces the v3 403/400/404 `{ok:false,error}` shapes exactly.

---

## 4. What does NOT work / the blocking problem

The v3 parity oracle is a **real `tmux attach`**, which needs a **PTY**. Getting a
PTY for `tmux attach` from the launchd daemon context is the blocker. Every
in-constraint route was exhausted:

1. **`script(1)` as a poor-man's PTY — DEAD on macOS.**
   `script -q /dev/null sh -c 'stty …; exec tmux attach -t S'` fails immediately
   with `script: tcgetattr/ioctl: Operation not supported on socket`. macOS
   `script` requires its *own* stdin to be a TTY; under the daemon (stdin is a
   pipe/socket) it can't run. No flag works around this. (Linux `util-linux
   script -c` would, but the fleet is macOS.)

2. **`openpty` via `libc`/`rustix` — blocked by the frozen-deps constraint.**
   `tokio` "full" gives `tokio::io::unix::AsyncFd` and `tokio::process`, but **not**
   `openpty`. evy-comms has no `libc`/`rustix`/`nix` direct dep. A true attach
   needs: add a PTY-capable dep (contract says *"do NOT touch workspace deps
   beyond what exists"*) + `unsafe` FFI (`openpty`) + `AsyncFd` wrapping of the
   master fd + `TIOCSWINSZ` ioctl for resize + careful child reaping. This is the
   clean path but it is **explicitly out-of-contract this session** and is
   substantial new unsafe systems code.

3. **`pipe-pane` + `send-keys` (no-dep) — input is unreliable; fidelity is degraded.**
   Streaming works (§3), but the input half via `send-keys` is not parity-grade:
   - **Input is flaky.** Injecting `echo …<CR>` into a live **fish** shell
     intermittently failed to execute / land — fish's readline + syntax
     highlighting + bracketed-paste mode swallows or reorders injected bytes.
     This is **independently corroborated by the subctl team's own finding**:
     `components/evy/handoff-directive.ts:6-10` — *"directives sent through raw
     tmux `send-keys` exhibit body-shape drift — newlines wrap, embedded
     characters get mangled… longer manual directives failed repeatedly."* They
     moved directives off `send-keys` onto a handoff-file transport for exactly
     this reason. A keystroke-by-keystroke web terminal cannot tolerate that.
   - **No real resize.** `pipe-pane` has no client, so the browser's `cols/rows`
     can't size the pane; `resize-window` would mutate the session globally.
   - **No tmux client rendering.** `pipe-pane` streams the *program's* bytes, not
     an attached client's view (no status bar; alt-screen apps approximate).

   Net: a demo could stream output and limp some input, but it is **not** a
   reliable v3-parity terminal — it would mangle the operator's keystrokes in the
   exact stuck-worker scenario the escape hatch exists to rescue.

**Conclusion:** reliable parity requires a real PTY attach; the only in-constraint
PTY route (macOS `script`) is dead, and the clean route (`openpty`) needs a
deps change the locked contract forbids. → tripwire.

---

## 5. Options + effort estimates

### Option A — `openpty` PTY attach (true parity). **Recommended.**
Add one PTY dep, do a real `tmux attach`.
- Add `libc = "0.2"` (already resolved transitively in the lockfile — no new
  crate is fetched) **or** enable `rustix` `["pty","termios"]`. Requires operator
  sign-off to lift the frozen-deps constraint for this one line.
- `unsafe { libc::openpty(...) }` → set master `O_NONBLOCK` → `tokio::io::unix::AsyncFd`
  for async read/write → spawn `tmux attach -t <session>` with the slave as
  stdin/stdout/stderr + `setsid`/controlling-tty → `TIOCSWINSZ` from the client's
  `cols/rows` (initial + on resize frames) → reap + detach on WS close.
- Browser protocol: decode `{type:data,b64}` → write to master; `{type:resize}` →
  `TIOCSWINSZ`; master bytes → binary WS frames. Direct map to v3.
- **Effort: ~1–1.5 days** (the unsafe FFI + AsyncFd + winsize + lifecycle are the
  cost; gate/enabled/teams are an hour using the Appendix). Highest fidelity,
  matches v3 byte-for-byte, reliable input.

### Option B — hyper WS reverse-proxy to Bun (keep Bun as the PTY host).
Stop trying to host the PTY in Rust; make v4 a *correct* WS proxy to Bun's
already-working `/api/terminal/attach` (Bun keeps node-pty).
- `reqwest` can't proxy a WS upgrade (that's the current gap). Use
  `tokio-tungstenite` (already a dep) to dial Bun's `/api/terminal/attach` and
  splice frames — the same pattern `proxy_http.rs::bridge_ws` already uses for
  `/api/live`, just pointed at the terminal path and made binary-clean.
- **Effort: ~2–4 hours.** Lowest risk, no PTY, no unsafe. **But** it is *not* a
  "native port" — Bun still owns the terminal. Good interim if the goal is just
  "terminal works through the v4 front door" rather than "Rust owns the terminal."

### Option C — `pipe-pane` + `send-keys` native (no deps). **Not recommended.**
Ship §3's streaming + `send-keys -H` input natively.
- **Effort: ~0.5 day**, zero deps. But input is unreliable (§4.3, corroborated by
  the team's own `handoff-directive.ts` lesson), no real resize, degraded
  rendering. Fails "reliable v3 parity"; would regress the escape-hatch's core
  job. Documented only for completeness.

**Recommendation:** **Option A** if the deliverable is genuinely "Rust owns the
terminal" — get the one-line deps waiver from the operator, then it's a clean,
reliable, true-parity port. If the operator would rather not lift the deps freeze
and just wants the panel functional behind the v4 door now, **Option B** is the
fast, safe interim (Bun stays the PTY host) and Option A can land later.

---

## 6. Where it wires (for whoever lands it)

`crates/evy-comms/src/http.rs::build_router` — register **above** the
`/api/{*rest}` catch-all (specific route wins):

```rust
.route("/api/terminal/enabled", get(crate::terminal_ws::enabled_handler))
.route("/api/terminal/teams",   get(crate::terminal_ws::teams_handler))
.route("/api/terminal/attach",  get(crate::terminal_ws::attach_handler)) // WS
```

`reverse_proxy_handler` / `ws_proxy_handler` / `proxy_http.rs` must **not** be
modified — the native routes simply pre-empt the catch-all. The `/api/live`
liveness path is unrelated and stays as-is.

Tests: bind an ephemeral port, create a throwaway `w8-test-*` tmux session, drive
a scripted WS client (axum test client or `tokio-tungstenite`), assert: live pane
bytes flow, keystrokes land, and flag-off / bad-team / missing-session return the
v3 `{ok:false,error}` shapes with 403/400/404.

---

## 7. Appendix — ready-to-use pure gate/enabled/teams (no deps)

Drop-in starting point for whichever option lands. Pure, no-PTY, exact v3 shapes.
Unit-testable with an injected lister + `SUBCTL_TERMINAL_FLAG_FILE` env.

```rust
//! Native web-terminal gate + enabled/teams handlers (v3 parity, Layer 2).
//! NOTE: flag-file gate only — NO HMAC (see TERMINAL_SPIKE.md §2).

use std::path::PathBuf;
use axum::{response::Json, http::StatusCode};
use serde_json::json;

/// `~/.config/subctl/terminal.enabled` (env overrides mirror v3 terminal.ts).
pub(crate) fn terminal_flag_path() -> PathBuf {
    if let Ok(p) = std::env::var("SUBCTL_TERMINAL_FLAG_FILE") {
        return PathBuf::from(p);
    }
    let cfg = std::env::var("SUBCTL_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let base = std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
                        .join(".config")
                });
            base.join("subctl")
        });
    cfg.join("terminal.enabled")
}

pub(crate) fn terminal_enabled() -> bool {
    terminal_flag_path().exists()
}

/// `GET /api/terminal/enabled` — always 200.
pub(crate) async fn enabled_handler() -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "enabled": terminal_enabled(),
        "flag_path": terminal_flag_path().to_string_lossy(),
    }))
}

#[derive(serde::Serialize)]
pub(crate) struct AttachableTeam {
    pub name: String,
    pub session: String,
    pub attached: bool,
}

/// `tmux list-sessions -F '#{session_name}\t#{session_attached}'`.
pub(crate) async fn default_tmux_lister() -> Vec<AttachableTeam> {
    // Reuse evy-providers' absolute tmux_bin resolution (launchd PATH gotcha)
    // by promoting a lister there, or inline the same probe here.
    let out = tokio::process::Command::new(tmux_bin())
        .args(["list-sessions", "-F", "#{session_name}\t#{session_attached}"])
        .output().await;
    let Ok(out) = out else { return vec![] };
    if !out.status.success() { return vec![]; }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() { return None; }
            let (name, attached) = l.split_once('\t')?;
            if name.is_empty() { return None; }
            Some(AttachableTeam {
                name: name.to_string(),
                session: name.to_string(),
                attached: attached != "0",
            })
        })
        .collect()
}

fn tmux_bin() -> String {
    if let Ok(p) = std::env::var("EVY_TMUX_BIN") { if !p.is_empty() { return p; } }
    for p in ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"] {
        if std::path::Path::new(p).exists() { return p.to_string(); }
    }
    "tmux".to_string()
}

/// `GET /api/terminal/teams` — 403 when disabled, else `{ok,count,teams}`.
pub(crate) async fn teams_handler() -> (StatusCode, Json<serde_json::Value>) {
    if !terminal_enabled() {
        return (StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": "terminal disabled" })));
    }
    let teams = default_tmux_lister().await;
    (StatusCode::OK, Json(json!({ "ok": true, "count": teams.len(), "teams": teams })))
}

/// Pre-upgrade gate decision (v3 evaluateUpgrade). Returns Err((status,reason))
/// → caller emits `{ok:false,error:reason}`; Ok(session) → proceed to attach.
pub(crate) fn evaluate_upgrade(team: Option<&str>) -> Result<String, (StatusCode, String)> {
    if !terminal_enabled() {
        return Err((StatusCode::FORBIDDEN, "terminal disabled".into()));
    }
    // (host-header check happens in the handler from req headers — see terminal.ts originAllowed)
    let team = team.unwrap_or("");
    if team.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "team query parameter required".into()));
    }
    if !team.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || team.len() > 128
    {
        return Err((StatusCode::BAD_REQUEST, "team name has invalid characters".into()));
    }
    // existence check uses the lister (async in the real handler):
    //   if !lister().iter().any(|s| s.name == team) → 404 "tmux session not found: {team}"
    Ok(team.to_string())
}
```

---

*W8, v4-parity-sprint. Spike only — no behaviour changed; terminal family stays
proxied to Bun. Recommendation: Option A (with a one-line deps waiver) for a true
native port, or Option B for a fast functional interim.*
