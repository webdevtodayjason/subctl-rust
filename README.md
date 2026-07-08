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

**Built and live.** 15 crates, ~1,240 test fns, zero `todo!()`/`unimplemented!()`
stubs. The daemon runs as `com.subctl.evy-v4` (HTTP/SSE `:8797`) on two nodes;
7/7 cutover criteria green (2026-06-09). Evy agency: EA1 (8 tools) live since
2026-06-11; EA2 (32 tools total) merged at `9b3b534` on 2026-06-15. Remaining
v3 parity tracked in [`PARITY_CENSUS.md`](./PARITY_CENSUS.md).

See ADR 0020 for the contract each crate implements.

## License

MIT — see [`LICENSE`](./LICENSE).
