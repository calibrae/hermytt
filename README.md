# hermytt

Transport-agnostic terminal multiplexer. One PTY session, any client that speaks text is a terminal.

```
                    ┌─── REST API (/stdin, /stdout SSE, /exec)
                    ├─── WebSocket (bidirectional, xterm.js web UI)
PTY (bash/zsh) ────┼─── MQTT (hermytt/{id}/in, hermytt/{id}/out)
                    ├─── Raw TCP (netcat-compatible)
                    └─── Telegram (bot, direct exec)
```

The hermit lives alone. But he talks to everyone.

## Quick start

```bash
# generate a config
hermytt-server example-config > hermytt.toml

# generate an auth token and paste it into [auth] token =
hermytt-server gen-token

# start
hermytt-server start -c hermytt.toml
```

Open `http://localhost:7777?token=YOUR_TOKEN` for the web terminal.
Open `http://localhost:7777/admin?token=YOUR_TOKEN` for the admin dashboard.

## Two execution models

**Stream transports** (WebSocket, TCP) pipe raw PTY I/O. Full terminal — colors, tab completion, TUIs, everything. Use the web UI or netcat.

**Request transports** (REST `/exec`, MQTT, Telegram) run commands directly via `sh -c`, bypassing the PTY entirely. Clean stdout/stderr/exit code, no ANSI junk. Use for automation, bots, quick commands.

REST also has stream endpoints (`/stdin`, `/stdout` SSE) for when you need the PTY.

## Transports

| Transport | Mode | Auth | Use case |
|-----------|------|------|----------|
| REST + WS | stream + exec | `X-Hermytt-Key` header or `?token=` | Web UI, API |
| MQTT | exec | Broker auth | IoT, home automation |
| TCP | stream | First-line token | netcat, scripts |
| Telegram | exec | chat_id whitelist | Quick commands from phone |

### REST

```bash
# execute a command (direct, no PTY)
curl -X POST http://host:7777/exec \
  -H 'Content-Type: application/json' \
  -H 'X-Hermytt-Key: TOKEN' \
  -d '{"input": "uptime"}'
# returns: {"stdout": "...", "stderr": "", "exit_code": 0}

# stream: send to PTY stdin
curl -X POST http://host:7777/stdin \
  -H 'Content-Type: application/json' \
  -H 'X-Hermytt-Key: TOKEN' \
  -d '{"input": "ls -la"}'

# stream: SSE output from PTY
curl -N http://host:7777/stdout?token=TOKEN

# sessions
curl -X POST http://host:7777/session?token=TOKEN     # create
curl http://host:7777/sessions?token=TOKEN              # list

# server info
curl http://host:7777/info?token=TOKEN
```

### WebSocket

```
ws://host:7777/ws?token=TOKEN             # default session
ws://host:7777/ws/SESSION_ID?token=TOKEN  # specific session
```

### MQTT

```bash
# send command (direct exec, response on /out)
mosquitto_pub -t hermytt/default/in -m "uptime"

# receive output
mosquitto_sub -t hermytt/default/out
```

### TCP

```bash
nc host 7779
# type token, press enter, then you're in a full terminal
```

### Telegram

Send any text to the bot — runs as a shell command, replies with output in a code block.

### Web UI

- `/` — tabbed xterm.js terminal (full PTY)
- `/admin` — admin dashboard (sessions, transports, quick exec)
- Mobile: input bar with Enter/Ctrl-C/Tab buttons
- **Ctrl+Shift+T** — new tab
- **Ctrl+Shift+W** — close tab
- **Ctrl+Shift+[/]** — switch tabs

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

All transports are optional — only include the ones you want.

## CLI

```
hermytt-server start [-c config.toml] [-s /bin/zsh] [-b 0.0.0.0]
hermytt-server gen-token
hermytt-server example-config
```

## Architecture

```
hermytt-core/         PTY sessions, output buffering, direct exec, platform abstraction
hermytt-transport/    Transport trait + REST/WS, MQTT, TCP, Telegram
hermytt-web/          Web UI (terminal + admin dashboard)
hermytt-server/       Config, CLI, transport wiring
hermytt-cli/          Client CLI (planned)
```

## Testing

```bash
cargo test            # 48 tests: unit + integration
```

## Building

```bash
cargo build --release
# binary at target/release/hermytt-server (~2MB)
```

Works on macOS and Linux. Windows support via `portable-pty` (defaults to PowerShell).

## License

MIT
