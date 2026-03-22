# hermytt

Transport-agnostic terminal multiplexer. One PTY, any client that speaks text.

```
                    ┌─── REST API (/stdin, /stdout SSE, /exec)
                    ├─── WebSocket (full terminal, crytter WASM UI)
PTY (bash/zsh) ────┼─── MQTT (command → response)
                    ├─── Raw TCP (netcat-compatible)
                    └─── Telegram bot (command → response)
```

The hermit lives alone. But he talks to everyone.

## Install

```bash
cargo install --path hermytt-server
```

Or build from source:

```bash
cargo build --release
# → target/release/hermytt-server (3MB)
```

Single static binary. No runtime dependencies. Web UI embedded.

## Quick start

```bash
# generate config + auth token
hermytt-server example-config > hermytt.toml
hermytt-server gen-token
# paste token into hermytt.toml under [auth]

# start
hermytt-server start -c hermytt.toml
```

Open `http://localhost:7777?token=YOUR_TOKEN` — full terminal in your browser.

Open `http://localhost:7777/admin?token=YOUR_TOKEN` — manage sessions and transports.

## Two modes

**Stream** (WebSocket, TCP) — full interactive terminal. Colors, tab completion, TUIs, vim, htop. Use the web UI or netcat.

**Exec** (REST `/exec`, MQTT, Telegram) — run a command, get stdout/stderr/exit code. Clean, fast, no PTY overhead. Use for automation and bots.

## Transports

| Transport | Mode | Auth |
|-----------|------|------|
| REST + WebSocket | both | `X-Hermytt-Key` header |
| MQTT | exec | broker auth |
| TCP | stream | first-line token |
| Telegram | exec | chat_id whitelist |

### REST

```bash
# execute a command (no PTY, clean output)
curl -X POST http://host:7777/exec \
  -H 'X-Hermytt-Key: TOKEN' \
  -H 'Content-Type: application/json' \
  -d '{"input": "uptime"}'
# → {"stdout":"...","stderr":"","exit_code":0}

# PTY stream: send to stdin
curl -X POST http://host:7777/stdin \
  -H 'X-Hermytt-Key: TOKEN' \
  -H 'Content-Type: application/json' \
  -d '{"input": "ls -la"}'

# PTY stream: SSE output
curl -N http://host:7777/stdout -H 'X-Hermytt-Key: TOKEN'

# sessions
curl -X POST http://host:7777/session -H 'X-Hermytt-Key: TOKEN'
curl http://host:7777/sessions -H 'X-Hermytt-Key: TOKEN'
```

### WebSocket

```
ws://host:7777/ws              → default session
ws://host:7777/ws/SESSION_ID   → specific session
```

First message: send auth token. Server replies `auth:ok`.

Resize: send `{"resize":[cols,rows]}` as a text message.

### MQTT

```bash
mosquitto_pub -t hermytt/default/in -m "uptime"
mosquitto_sub -t hermytt/default/out
```

### TCP

```bash
nc host 7779
# type token, press enter, full terminal
```

### Telegram

Send any text to the bot — runs as shell command, replies with output.

## Web UI

The terminal uses [crytter](https://github.com/cali/crytter) — a Rust/WASM terminal emulator compiled to 86KB. No xterm.js, no JavaScript terminal parsing. Zero external dependencies.

- `/` — tabbed terminal (full PTY, keyboard shortcuts)
- `/admin` — session management, transport config, quick exec
- Mobile: input bar with Enter/Ctrl-C/Tab buttons
- `Ctrl+Shift+T` new tab, `Ctrl+Shift+W` close, `Ctrl+Shift+[/]` switch

## Config

```toml
[server]
bind = "127.0.0.1"
shell = "/bin/zsh"
scrollback = 1000

[auth]
token = "your-secret-token"

[transport.rest]
port = 7777

[transport.mqtt]
broker = "mqtt.example.com"
port = 1883
username = "mqtt"
password = "secret"

[transport.tcp]
port = 7779

[transport.telegram]
bot_token = "your-bot-token"
chat_ids = [123456789]
```

All transports optional. Only include what you need.

## CLI

```
hermytt-server start [-c config.toml] [-s /bin/zsh] [-b 0.0.0.0]
hermytt-server gen-token
hermytt-server example-config
```

## Architecture

```
hermytt-core/         PTY sessions, output buffering, direct exec, platform
hermytt-transport/    Transport trait + REST/WS, MQTT, TCP, Telegram
hermytt-web/          Web UI (terminal + admin), crytter WASM
hermytt-server/       Config, CLI, wiring
hermytt-cli/          Client CLI (planned)
```

## Testing

```bash
cargo test              # 52 unit + integration tests
npx playwright test     # browser e2e tests
```

Integration tests use embedded MQTT broker — no external infra.

## Cross-compile

```bash
# Linux static binary (from macOS)
./deploy.sh
```

Requires `musl-cross` (`brew install musl-cross`).

## Security

- Auth on all transports (header, first-message WS, first-line TCP, broker/chat whitelist)
- Default bind `127.0.0.1`
- Exec: 30s timeout, 1MB output cap, 8 concurrent max
- Session limit (default 16)
- Token stored in sessionStorage, stripped from URL
- Three OWASP audit passes

## License

MIT — see [LICENSE](LICENSE).
