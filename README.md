# hermytt

Transport-agnostic terminal multiplexer. One PTY, any client that speaks text.

```
                    ┌─── REST API (/stdin, /stdout SSE, /exec)
                    ├─── WebSocket (bidirectional, full terminal)
PTY (bash/zsh) ────┼─── MQTT (command → response)
                    └─── Raw TCP (netcat-compatible)
```

The hermit lives alone. But he talks to everyone.

## Install

```bash
cargo install --path hermytt-server
```

Or build from source:

```bash
cargo build --release
# → target/release/hermytt-server (~4MB)
```

Single static binary. No runtime dependencies.

## Quick start

```bash
# generate config + auth token
hermytt-server example-config > hermytt.toml
hermytt-server gen-token
# paste token into hermytt.toml under [auth]

# start
hermytt-server start -c hermytt.toml
```

## Two execution models

**Stream** (WebSocket, TCP) — full interactive terminal. Colors, tab completion, TUIs, vim, htop. Connect with any WebSocket client or netcat.

**Exec** (REST `/exec`, MQTT) — run a command, get stdout/stderr/exit_code. Clean, fast, no PTY. Use for automation, bots, scripts.

## Transports

| Transport | Mode | Auth |
|-----------|------|------|
| REST + WebSocket | both | `X-Hermytt-Key` header |
| MQTT | exec | broker auth |
| TCP | stream | first-line token |

### REST

```bash
# execute a command (direct, no PTY)
curl -X POST http://host:7777/exec \
  -H 'X-Hermytt-Key: TOKEN' \
  -H 'Content-Type: application/json' \
  -d '{"input": "uptime"}'
# → {"stdout":"...","stderr":"","exit_code":0}

# PTY: send to stdin / stream output (SSE)
curl -X POST http://host:7777/stdin -H 'X-Hermytt-Key: TOKEN' \
  -H 'Content-Type: application/json' -d '{"input": "ls -la"}'
curl -N http://host:7777/stdout -H 'X-Hermytt-Key: TOKEN'

# sessions
curl -X POST http://host:7777/session -H 'X-Hermytt-Key: TOKEN'
curl http://host:7777/sessions -H 'X-Hermytt-Key: TOKEN'

# file transfer
curl -X POST http://host:7777/files/upload -H 'X-Hermytt-Key: TOKEN' \
  -F 'file=@local.txt'
curl http://host:7777/files -H 'X-Hermytt-Key: TOKEN'
curl http://host:7777/files/local.txt -H 'X-Hermytt-Key: TOKEN' -o local.txt

# session recording
curl -X POST http://host:7777/session/ID/record -H 'X-Hermytt-Key: TOKEN'
curl -X POST http://host:7777/session/ID/stop-record -H 'X-Hermytt-Key: TOKEN'
curl http://host:7777/recordings -H 'X-Hermytt-Key: TOKEN'
# recordings are asciicast v2, playable with: asciinema play file.cast
```

### WebSocket

```
ws://host:7777/ws              → default session
ws://host:7777/ws/SESSION_ID   → specific session
```

First message: send auth token. Server replies `auth:ok`.

Control messages (JSON, intercepted before PTY):
- `{"resize":[cols,rows]}` — resize PTY
- `{"paste_image":{"name":"file.png","data":"base64..."}}` — save image to files_dir

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

## Web UI (optional)

Hermytt includes an optional embedded web UI powered by [crytter](https://github.com/calibrae/crytter) (86KB WASM terminal emulator) and [prytty](https://github.com/calibrae/prytty) (75KB WASM syntax highlighter). The web UI is decoupled from the core — hermytt works headless without it.

When enabled: tabbed terminal at `/`, admin dashboard at `/admin`.

## Related projects

| Project | Role |
|---------|------|
| [crytter](https://github.com/calibrae/crytter) | Rust/WASM terminal emulator (86KB) |
| [prytty](https://github.com/calibrae/prytty) | Rust/WASM syntax highlighting (75KB) |
| [hermytt-bots](https://github.com/calibrae/hermytt-bots) | Chat bot bridges (Telegram, Signal, Discord) |
| [fytti](https://github.com/calibrae/fytti) | WASM app player (hosts crytter + prytty) |
| [wytti](https://github.com/calibrae/wytti) | WASI runtime (sandboxed exec backend) |

## Config

```toml
[server]
bind = "127.0.0.1"
shell = "/bin/zsh"
scrollback = 1000
# tls_cert = "/path/to/cert.pem"
# tls_key = "/path/to/key.pem"
# recording_dir = "/tmp/hermytt-recordings"
# auto_record = false
# files_dir = "/tmp/hermytt-files"

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
hermytt-core/         PTY sessions, output buffering, direct exec, recording
hermytt-transport/    Transport trait + REST/WS, MQTT, TCP (pure API, no web)
hermytt-web/          Optional web UI (decoupled, injectable)
hermytt-server/       Config, CLI, transport wiring
```

## Testing

```bash
cargo test              # 48 unit + integration tests
npx playwright test     # browser e2e tests (when web UI enabled)
```

Integration tests use an embedded MQTT broker — no external infra needed.

## Cross-compile

```bash
# Linux static binary (from macOS)
./deploy.sh
```

Requires `musl-cross` (`brew install musl-cross`).

## Security

- Auth on all transports (header, first-message WS, first-line TCP, broker auth)
- Default bind `127.0.0.1`
- TLS support (rustls)
- Exec: 30s timeout, 1MB output cap, 8 concurrent max
- Session limit (default 16)
- File upload size limit (default 10MB)
- Path traversal protection on all file operations
- Multiple OWASP audit passes

## License

MIT — see [LICENSE](LICENSE).
