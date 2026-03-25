# hermytt

Transport-agnostic terminal multiplexer. One PTY, any client that speaks text.

```
                    ┌─── REST API (/stdin, /stdout, /exec)
                    ├─── WebSocket (full terminal)
PTY (bash/zsh) ────┼─── MQTT (command → response)
                    └─── Raw TCP (netcat-compatible)
```

The hermit lives alone. But he talks to everyone.

## Install

Download a binary from [releases](https://github.com/yttfam/hermytt/releases):

```bash
curl -fSL https://github.com/yttfam/hermytt/releases/latest/download/hermytt-server-linux-x86_64 -o hermytt-server
chmod +x hermytt-server
```

Or build from source:

```bash
cargo build --release
```

Single static binary. Zero runtime dependencies. ~4-7MB depending on platform.

## Quick start

```bash
hermytt-server example-config > hermytt.toml
hermytt-server gen-token
# paste token into hermytt.toml under [auth]

hermytt-server start -c hermytt.toml
```

## How it works

Hermytt has two execution models:

**Stream** (WebSocket, TCP) — full bidirectional PTY. Interactive terminal with colors, tab completion, TUIs, vim, htop. Connect with any WebSocket client or `nc`.

**Exec** (REST `/exec`, MQTT) — run a command, get `stdout`/`stderr`/`exit_code`. No PTY, no ANSI, no junk. For automation, bots, and scripts.

## Transports

| Transport | Mode | Auth |
|-----------|------|------|
| REST + WebSocket | both | `X-Hermytt-Key` header |
| MQTT | exec | broker auth |
| TCP | stream | first-line token |

### REST

```bash
# Direct exec (no PTY)
curl -X POST http://host:7777/exec \
  -H 'X-Hermytt-Key: TOKEN' \
  -H 'Content-Type: application/json' \
  -d '{"input": "uptime"}'
# → {"stdout":"...","stderr":"","exit_code":0}

# PTY stream
curl -X POST http://host:7777/stdin -H 'X-Hermytt-Key: TOKEN' \
  -H 'Content-Type: application/json' -d '{"input": "ls"}'
curl -N http://host:7777/stdout -H 'X-Hermytt-Key: TOKEN'

# Sessions
curl -X POST http://host:7777/session -H 'X-Hermytt-Key: TOKEN'
curl http://host:7777/sessions -H 'X-Hermytt-Key: TOKEN'

# File transfer
curl -X POST http://host:7777/files/upload -H 'X-Hermytt-Key: TOKEN' -F 'file=@local.txt'
curl http://host:7777/files/local.txt -H 'X-Hermytt-Key: TOKEN' -o local.txt

# Recording (asciicast v2, playable with asciinema)
curl -X POST http://host:7777/session/ID/record -H 'X-Hermytt-Key: TOKEN'
curl http://host:7777/recordings -H 'X-Hermytt-Key: TOKEN'
```

### WebSocket

```
ws://host:7777/ws              → default session
ws://host:7777/ws/SESSION_ID   → specific session
```

First message: auth token. Server replies `auth:ok`. Then bidirectional PTY I/O.

Control messages (JSON, intercepted before PTY):
- `{"resize":[cols,rows]}` — resize terminal
- `{"paste_image":{"name":"file.png","data":"base64..."}}` — save image
- `{"exit":true}` — sent by server when shell exits

### MQTT

```bash
mosquitto_pub -t hermytt/default/in -m "uptime"
mosquitto_sub -t hermytt/default/out
```

### TCP

```bash
nc host 7779
# type token, press enter, you're in
```

## Web UI (optional, embedded)

Hermytt ships with an optional web interface powered by [crytter](https://github.com/yttfam/crytter) (86KB WASM terminal) and [prytty](https://github.com/yttfam/prytty) (75KB WASM syntax highlighter). The web UI is decoupled from the core — hermytt works headless.

- `/` — tabbed terminal with keyboard shortcuts
- `/admin` — command center: family services, sessions, transports, exec, config

## The YTT Family

Hermytt is the patriarch of an ecosystem of composable tools:

| Project | Role |
|---------|------|
| **hermytt** | Transport multiplexer — routes bytes, auth, sessions |
| [shytti](https://github.com/yttfam/shytti) | Shell orchestrator — spawns and manages shells |
| [crytter](https://github.com/yttfam/crytter) | WASM terminal emulator (86KB) |
| [prytty](https://github.com/yttfam/prytty) | WASM syntax highlighter (75KB) |
| [fytti](https://github.com/yttfam/fytti) | GPU-accelerated WASM app runtime |
| [wytti](https://github.com/yttfam/wytti) | WASI sandbox runtime |
| [hermytt-bots](https://github.com/yttfam/hermytt-bots) | Chat bot bridges (Telegram, Signal, Discord) |

### Service registry

Family members announce themselves to hermytt and appear in the admin dashboard:

```bash
curl -X POST http://host:7777/registry/announce \
  -H 'X-Hermytt-Key: TOKEN' \
  -H 'Content-Type: application/json' \
  -d '{"name":"shytti-mista","role":"shell","endpoint":"ws://10.10.0.3:7778"}'

curl http://host:7777/registry -H 'X-Hermytt-Key: TOKEN'
```

### Bootstrap

Deploy Shytti to a new machine with one command:

```bash
curl -H 'X-Hermytt-Key: TOKEN' http://hermytt:7777/bootstrap/shytti | sudo bash
```

Downloads binary, writes config, installs systemd service, starts it.

### Internal session API (for Shytti)

Shytti manages shells externally and connects them through hermytt:

```
POST   /internal/session           → register managed session
DELETE /internal/session/{id}      → unregister
WS     /internal/session/{id}/pipe → bidirectional PTY bridge
```

If no shell orchestrator registers within 5 seconds, hermytt falls back to spawning a local PTY.

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
token = "your-token"

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

All transports optional.

## CLI

```
hermytt-server start [-c config.toml] [-s /bin/zsh] [-b 0.0.0.0]
hermytt-server gen-token
hermytt-server example-config
```

## Architecture

```
hermytt-core/         PTY sessions, exec, recording, service registry
hermytt-transport/    REST/WS, MQTT, TCP, TLS (pure API, no web)
hermytt-web/          Optional embedded web UI (decoupled)
hermytt-server/       Config, CLI, wiring
```

## Security

- Auth on all transports (header, first-message WS, first-line TCP, broker)
- Default bind `127.0.0.1`
- TLS support (rustls)
- Exec: 30s timeout, 1MB output cap, 8 concurrent max
- Session limit (default 16), file upload limit (default 10MB)
- Path traversal protection, XSS escaping
- Multiple OWASP audit passes

## Testing

```bash
cargo test              # 55 unit + integration tests
npx playwright test     # browser e2e tests
```

Integration tests spin up an embedded MQTT broker — no external infra.

## Cross-compile & deploy

```bash
./deploy.sh   # builds linux-musl, scp to mista, restarts systemd
```

## License

MIT
