# Project Instructions

## Product boundary

- This repository implements a remote message console for humans using Herdr. One Rust `agent-talk` binary serves an embedded Svelte browser app and a small HTTP API.
- Herdr owns live destinations and lifecycle state. Codex and Claude Code own their native session transcripts. Messages go verbatim into the selected existing CLI session; replies are read from that session's transcript.
- Peer communication, MCP, broker RPC, mailboxes, delivery journals, acknowledgements, reminders, and worker orchestration are removed. Do not reintroduce parallel message storage or interpret a person's instruction with another AI.
- HTTP listens on `0.0.0.0` using `PORT` (default `5002`, digits in `1..=65535`; invalid values fail startup). The operator owns the Tailscale/proxy access boundary. Same-privilege processes can also invoke the API; this process does not guarantee human-only access.

## Architecture

- `src/main.rs` owns daemon/update command dispatch. `src/config.rs` owns environment discovery.
- `src/daemon.rs` owns HTTP input validation, origin checks, embedded assets, and connection handling.
- `src/herdr.rs` owns bounded Herdr CLI execution and checks the expected native session and foreground process immediately before prompting. Rejected, missing, unregistered, unknown, or blocked destinations receive no input. Input acknowledgement is not work completion; lost acknowledgements are indeterminate and must not be automatically retried.
- The read-only `/api/screen` uses Herdr `pane read --source visible --format text` output, checks terminal identity, and marks stale display in the UI. Reading does not require native registration or permission to send. It never forwards terminal input or replaces native history.
- `src/history.rs` reads bounded native Codex/Claude transcripts. Do not accept arbitrary user-supplied file paths or replace reports with terminal screenshots.
- `src/update.rs` verifies release checksums and replaces the executable. The operator restarts the managed service.
- `DESIGN.md` owns browser interaction and visual design. Preserve draft text and keep it bound to the selected CLI session. Mobile users must not need terminal modifier keys.

## Verification

Run from the repository root:

```sh
npm --prefix client ci
npm --prefix client run format:check
npm --prefix client run check
npm --prefix client test
npm --prefix client run build
cargo fmt -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo test --locked --test remote
cargo build --locked --release
```

Build the frontend before Rust so `client/dist` is embedded. Cargo itself does not invoke npm; builds without client assets serve API routes but return 503 for the UI.

Use Chromium + Playwright for browser E2E. Integration tests isolate HTTP and replace the Herdr executable with a Python 3 CLI fixture. Changes to live input or transcript adapters also require dedicated real Codex/Claude sessions: select in the browser, submit original text, verify actual CLI receipt and visible assistant output. Never use another person's pane for tests.
