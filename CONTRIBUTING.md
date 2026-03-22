# Contributing to hermytt

## Getting started

```bash
git clone https://github.com/cali/hermytt
cd hermytt
cargo build
cargo test
```

Requires Rust 2024 edition (stable 1.85+).

## Running locally

```bash
hermytt-server example-config > hermytt.toml
# edit hermytt.toml — at minimum add [auth] token = "..."
hermytt-server start -c hermytt.toml
```

## Project structure

```
hermytt-core/         PTY sessions, output buffering, direct exec, platform
hermytt-transport/    Transport trait + REST/WS, MQTT, TCP, Telegram
hermytt-web/          Web UI (terminal + admin), vendored crytter WASM
hermytt-server/       Config, CLI, transport wiring
hermytt-cli/          Client CLI (planned)
tests-e2e/            Playwright browser tests
```

## Testing

```bash
cargo test                                    # unit + integration (52 tests)
npx playwright test tests-e2e/                # browser e2e tests
```

Integration tests spin up real servers on random ports and an embedded MQTT broker — no external infra needed.

## Adding a transport

1. Create `hermytt-transport/src/yourtransport.rs`
2. Implement the `Transport` trait
3. Add config struct in `hermytt-server/src/config.rs`
4. Wire it in `hermytt-server/src/main.rs`

Two execution models:
- **Stream** (WebSocket, TCP): pipe raw PTY I/O, full terminal
- **Request** (REST /exec, MQTT, Telegram): `hermytt_core::exec()` for clean command → output

## Style

- Keep it simple. No over-engineering.
- `cargo clippy` clean.
- Tests for anything non-trivial.
- Security: auth everything, validate inputs, no unbounded allocations.

## Pull requests

- One concern per PR.
- Describe what and why, not how.
- Tests required for new features and bug fixes.
