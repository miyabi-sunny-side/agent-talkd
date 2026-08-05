# Project Instructions

## Product boundary

- This repository implements a herdr-native message broker in Rust. The shipped executable is the single `agent-talk` binary; its CLI and daemon modes share that binary.
- The supported multiplexer is herdr. One daemon owns registrations, busy/idle state, delivery queues, and journal recovery in one event loop, and it listens on one Unix domain socket derived from the herdr socket. Clients derive the same socket from their own environment and all reach the same process. (tmux support was removed after the herdr migration.)
- Keep the product local to the same host. Do not treat self-reported pane/source metadata as an authentication boundary.
- The daemon may expose the read-only HTTP surface over TCP when `AGENT_TALK_HTTP_ADDR` is set (default: off). This is the mobile access path; the network boundary is owned by the operator's VPN, not by this process. Do not add credential handling or an authentication layer here without a new user decision.
- The repository includes a Svelte status client under `client/`. It is a read-only view over the daemon's HTTP adapter.

## Architecture and invariants

- `src/main.rs` is the command dispatcher. Keep user-facing command parsing and help consistent with `src/help.rs` and the request handling in `src/client.rs`.
- `src/daemon.rs` coordinates RPC and health-tick events; `src/state.rs` owns delivery state transitions; `src/journal.rs` owns durable append/recovery/checkpoint behavior; `src/backend.rs` adapts herdr panes to the addressing surface; `src/herdr.rs` isolates the herdr API; `src/lifecycle.rs` manages daemon discovery and replacement.
- Treat daemon memory as the live source of truth for delivery state. Registration follows herdr's native agent identity through the pull sync; there is no mirror state.
- Never inject keystrokes into a pane that is not positively known to be idle. herdr reports `blocked` for approval dialogs; a pane whose status is unknown is not idle.
- Preserve the delivery durability contract: persist and `fsync` a message before reporting it as sent or queued, recover unread messages and queued deliveries after restart, and never reuse message IDs after checkpointing.
- Keep terminal injection limited to daemon-generated, validated notification text. Message bodies remain in the journal and are retrieved with `agent-talk read <id>`.
- Preserve backward-compatibility behavior deliberately. Protocol additions that older daemons cannot interpret must fail explicitly instead of silently degrading to a different command.
- Keep platform-dependent behavior behind the existing multiplexer, lifecycle, configuration, and update boundaries. Avoid scattering subprocess execution or filesystem-path discovery through command handlers.

## Change discipline

- Keep changes small and focused. Add or update tests alongside observable CLI, protocol, state-machine, journal, lifecycle, or delivery behavior.
- Do not hand-edit `Cargo.lock`; update it through Cargo when dependency metadata changes.
- Public behavior belongs in `README.md`; non-obvious delivery and persistence invariants belong in `docs/design.md`. Keep both aligned with implemented behavior.

## Verification

Run the following from the repository root:

```sh
npm --prefix client ci
npm --prefix client run format:check
npm --prefix client run check
npm --prefix client test
npm --prefix client run build
cargo fmt -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo test --locked --test bridge -- --ignored
cargo build --locked --release
```

Run the frontend build before the Rust checks so `client/dist` is embedded in the
binary under test. Cargo deliberately does not invoke npm, and remains buildable
without `client/dist`; in that case static HTTP routes return 503.

The ignored integration tests in `tests/bridge.rs` spawn a background daemon against a fake herdr socket, so they do not need herdr installed. Run every command before delivery; if the environment prevents one, report that command and the reason explicitly.
