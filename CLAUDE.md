You are building Hermytt — a transport-agnostic terminal multiplexer in Rust. A PTY session lives on the server, and any client that can send text can be a terminal: REST, MQTT, Telegram, Signal, WebSocket, raw TCP, carrier pigeon.

The hermit lives alone. But he talks to everyone.

## The Idea

```
                    ┌─── REST API (/stdin, /stdout)
                    ├─── MQTT (hermytt/in, hermytt/out)  
PTY (bash/zsh) ────┼─── WebSocket (ws://host:port)
                    ├─── Telegram (bot API)
                    └─── Raw TCP (netcat-friendly)
```

One PTY session. Multiple transports. Any client that speaks text is a terminal emulator.

`echo "ls -la" | curl -X POST http://server:7777/stdin` — that's a terminal.
`mosquitto_pub -t hermytt/in -m "uptime"` — that's also a terminal.
Sending "df -h" to a Telegram bot — terminal.
`nc server 7778` — terminal.

## Architecture

```
hermytt/
├── hermytt-core/         # PTY management, session lifecycle, I/O multiplexing
├── hermytt-transport/    # Transport trait + implementations
│   ├── rest.rs           # axum: POST /stdin, GET /stdout, SSE for streaming
│   ├── mqtt.rs           # rumqttc: sub hermytt/{session}/in, pub hermytt/{session}/out
│   ├── websocket.rs      # tokio-tungstenite: bidirectional
│   ├── tcp.rs            # raw TCP: netcat-compatible
│   └── telegram.rs       # teloxide or raw API: bot receives text, replies with output
├── hermytt-server/       # Main daemon: config, session manager, transport registry
└── hermytt-cli/          # CLI: hermytt start, hermytt attach, hermytt list
```

## Core Design

### PTY Session
- Uses `portable-pty` crate for cross-platform PTY
- Each session has an ID (short uuid like "a7f3")
- Spawns a shell (configurable: bash, zsh, sh)
- Ring buffer for scrollback (last N lines available to new clients)
- Session timeout (configurable, default: none for persistent sessions)

### Transport Trait
```rust
#[async_trait]
trait Transport: Send + Sync {
    /// Start listening for input and sending output
    async fn serve(&self, session: Arc<Session>) -> Result<()>;
    
    /// Transport name for logging
    fn name(&self) -> &str;
}
```

Each transport subscribes to the session's output stream and pushes input to the session's stdin. Multiple transports can be active simultaneously on the same session.

### Multi-Session
- Multiple PTY sessions can run simultaneously
- Each session gets its own namespace per transport:
  - REST: `/session/{id}/stdin`
  - MQTT: `hermytt/{id}/in`, `hermytt/{id}/out`
  - WebSocket: `ws://host:port/{id}`
- Default session if only one is running

### Output Chunking
Terminal output can be fast and voluminous. For slow transports (Telegram, MQTT):
- Buffer output for a configurable window (e.g., 200ms)
- Send as one message instead of character-by-character
- Respect transport message size limits (Telegram: 4096 chars)
- Truncate with "... (truncated, N bytes)" for massive outputs

## Config

```toml
[server]
bind = "0.0.0.0"
shell = "/bin/bash"
scrollback = 1000  # lines

[transport.rest]
enabled = true
port = 7777

[transport.websocket]
enabled = true
port = 7778

[transport.mqtt]
enabled = true
broker = "mqtt.example.com"
port = 1883
user = "mqtt"
# password from env or Vault

[transport.tcp]
enabled = true
port = 7779

[transport.telegram]
enabled = false
# bot_token from env or Vault
# chat_id = "123456789"
```

## Security

- REST: API key header (`X-Hermytt-Key`)
- MQTT: broker auth (already secured)
- WebSocket: token query param or first-message auth
- TCP: IP whitelist or token-first-line
- Telegram: chat_id whitelist (only respond to authorized users)
- All transports optional — disable what you don't need

## Tech Stack

- `portable-pty` — cross-platform PTY
- `tokio` — async runtime
- `axum` — REST + WebSocket
- `rumqttc` — MQTT client
- `serde` + `toml` — config
- `clap` — CLI
- `tracing` — structured logging
- `uuid` — session IDs

## Stretch Goals

- [ ] Session sharing — multiple users watching/typing in the same PTY
- [ ] Read-only mode — watch a session without input capability
- [ ] Recording — save session to asciicast format (asciinema-compatible)
- [ ] File transfer — upload/download files through the transport
- [ ] Shell detection — auto-detect prompt for smarter output chunking
- [ ] Signal transport — Signal bot via signal-cli
- [ ] Matrix transport — Matrix bot
- [ ] Web UI — minimal HTML page that connects via WebSocket

## Cali's Preferences

- Rust workspace, aggressive release profile
- Zero unsafe unless PTY internals demand it
- Ship REST + MQTT + WebSocket first, add Telegram later
- Config file, not just CLI flags
- Must work on macOS and Linux
- Small binary, fast startup, low memory
- The name is Hermytt. The hermit TTY. He lives alone but talks to everyone.
