# Project Instructions

## Product boundary

- This repository implements a tmux-native message broker in Rust. The shipped executable is the single `agent-talk` binary; its CLI and daemon modes share that binary.
- The daemon is scoped to one tmux server. It owns registrations, busy/idle state, delivery queues, and journal recovery in one event loop. CLI commands send requests over the Unix domain socket associated with that tmux server.
- Keep the product local to the same host and tmux server. Do not introduce a network service or treat self-reported pane/source metadata as an authentication boundary.
- There is no web or JavaScript client in the current product. Do not add frontend assumptions to Rust-only changes.

## Architecture and invariants

- `src/main.rs` is the command dispatcher. Keep user-facing command parsing and help consistent with `src/help.rs` and the request handling in `src/client.rs`.
- `src/daemon.rs` coordinates RPC and tmux events; `src/state.rs` owns delivery state transitions; `src/journal.rs` owns durable append/recovery/checkpoint behavior; `src/tmux.rs` isolates tmux interaction; `src/lifecycle.rs` manages daemon discovery and replacement.
- Treat daemon memory as the live source of truth. tmux `@agent` and `@agent_state` options are compatibility mirrors, not an independent state store.
- Preserve the delivery durability contract: persist and `fsync` a message before reporting it as sent or queued, recover unread messages and queued deliveries after restart, and never reuse message IDs after checkpointing.
- Keep terminal injection limited to daemon-generated, validated notification text. Message bodies remain in the journal and are retrieved with `agent-talk read <id>`.
- Preserve backward-compatibility behavior deliberately. Protocol additions that older daemons cannot interpret must fail explicitly instead of silently degrading to a different command.
- Keep platform-dependent behavior behind the existing tmux, lifecycle, configuration, and update boundaries. Avoid scattering subprocess execution or filesystem-path discovery through command handlers.

## Change discipline

- Keep changes small and focused. Add or update tests alongside observable CLI, protocol, state-machine, journal, lifecycle, or tmux behavior.
- Do not hand-edit `Cargo.lock`; update it through Cargo when dependency metadata changes.
- Public behavior belongs in `README.md`; non-obvious delivery and persistence invariants belong in `docs/design.md`. Keep both aligned with implemented behavior.

## Verification

Run the following from the repository root:

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo test --locked --test tmux_integration -- --ignored
cargo build --locked --release
```

The ignored integration test creates an isolated real tmux server and therefore requires tmux to be installed and executable. Run all five commands before delivery; if the environment prevents one, report that command and the reason explicitly.
