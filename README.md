# subctl-rust

The Evy v4 Rust rewrite. This workspace is a from-scratch reimplementation of
the subctl daemon (currently TypeScript on Bun, living in
`../subctl/`). The two trees coexist during cutover; this one is the
production target.

**Authoritative spec:**
[`docs/adr/0020-evy-v4-learning-orchestrator-spec.md`](../subctl/docs/adr/0020-evy-v4-learning-orchestrator-spec.md)
in the parent `subctl/` repo.

## Layout

```
crates/
├── evy-core/        — types, traits (Provider, Mandate, WorkerHandle), shared error type
├── evy-policy/      — policy gate (Trusted / Gated / Sealed)
├── evy-providers/   — Provider trait impls (Claude Code, Codex, DeepSeek)
├── evy-scheduler/   — cron-shaped job runner; persistent, survives restart
├── evy-comms/       — multi-channel router (HTTP/SSE, TUI, Discord, Telegram)
├── evy-memory/      — observation log (sqlx + SQLite), retrieval, learning-loop substrate
└── evy/             — daemon binary; wires the six libraries together
```

Seven crates corresponding to seven of the eight Evy v4 primitives. The
eighth — the learning loop — is materialized inside `evy-memory` rather than
as its own crate.

## Quick start

```bash
cargo check --workspace
cargo build --workspace --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Status

**Phase 0 — scaffold only.** Every `lib.rs` is a `// TODO: Phase 1+` stub.
The binary boots a tokio runtime, initializes `tracing`, logs a single
line, and exits. No primitives are wired yet.

See ADR 0020 for the contract each crate will implement.

## License

MIT — see [`LICENSE`](./LICENSE).
