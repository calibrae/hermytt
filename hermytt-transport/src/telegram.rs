use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use hermytt_core::SessionManager;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::Transport;

const SILENCE_TIMEOUT: Duration = Duration::from_millis(500);
const STREAM_BUFFER: Duration = Duration::from_millis(800);
const MAX_MESSAGE_LEN: usize = 4000;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Exec,
    Raw,
}

pub struct TelegramTransport {
    pub bot_token: String,
    pub chat_ids: Vec<i64>,
}

#[async_trait]
impl Transport for TelegramTransport {
    async fn serve(self: Arc<Self>, sessions: Arc<SessionManager>) -> Result<()> {
        let client = reqwest::Client::new();
        let base_url = format!("https://api.telegram.org/bot{}", self.bot_token);

        info!(transport = "telegram", "starting long poll");

        poll_loop(&client, &base_url, &self.chat_ids, &sessions).await;

        Ok(())
    }

    fn name(&self) -> &str {
        "telegram"
    }
}

#[derive(Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Deserialize)]
struct TgMessage {
    chat: TgChat,
    text: Option<String>,
}

#[derive(Deserialize)]
struct TgChat {
    id: i64,
}

#[derive(Serialize)]
struct SendMessageRequest<'a> {
    chat_id: i64,
    text: &'a str,
    parse_mode: Option<&'a str>,
}

/// Per-chat state for mode tracking, session attachment, and raw streaming.
struct ChatState {
    mode: Mode,
    /// Which session this chat is attached to. None = default session.
    session_id: Option<String>,
    /// Abort handle for the raw stream task (if running).
    raw_task: Option<tokio::task::JoinHandle<()>>,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            mode: Mode::Exec,
            session_id: None,
            raw_task: None,
        }
    }
}

impl ChatState {
    fn stop_raw(&mut self) {
        if let Some(task) = self.raw_task.take() {
            task.abort();
        }
    }
}

async fn poll_loop(
    client: &reqwest::Client,
    base_url: &str,
    allowed_chats: &[i64],
    sessions: &Arc<SessionManager>,
) {
    let poll_url = format!("{}/getUpdates", base_url);
    let send_url = format!("{}/sendMessage", base_url);
    let mut offset: i64 = 0;
    let chat_states: Arc<Mutex<HashMap<i64, ChatState>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        let resp = client
            .get(&poll_url)
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", "30".to_string()),
            ])
            .send()
            .await;

        let updates: Vec<TgUpdate> = match resp {
            Ok(r) => match r.json::<TgResponse<Vec<TgUpdate>>>().await {
                Ok(tg) if tg.ok => tg.result.unwrap_or_default(),
                Ok(_) => {
                    warn!(transport = "telegram", "API returned ok=false");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                Err(e) => {
                    error!(transport = "telegram", error = %e, "parse error");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            },
            Err(e) => {
                error!(transport = "telegram", error = %e, "poll failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.update_id + 1;

            let Some(msg) = update.message else { continue };
            let Some(text) = msg.text else { continue };
            let chat_id = msg.chat.id;

            if !allowed_chats.is_empty() && !allowed_chats.contains(&chat_id) {
                warn!(transport = "telegram", chat_id, "unauthorized chat");
                continue;
            }

            // Helper: resolve the session handle for this chat.
            let resolve_handle = |states: &HashMap<i64, ChatState>| -> Option<String> {
                states.get(&chat_id).and_then(|s| s.session_id.clone())
            };

            // Handle commands.
            if text.starts_with('/') {
                let parts: Vec<&str> = text.splitn(2, ' ').collect();
                let cmd = parts[0];
                let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

                match cmd {
                    "/raw" => {
                        let session_id = {
                            let states = chat_states.lock().await;
                            resolve_handle(&states)
                        };
                        let handle = match get_handle(sessions, session_id.as_deref()).await {
                            Some(h) => h,
                            None => { send_message(client, &send_url, chat_id, "no session attached").await; continue; }
                        };
                        let mut states = chat_states.lock().await;
                        let state = states.entry(chat_id).or_default();
                        state.stop_raw();
                        state.mode = Mode::Raw;

                        let stream_client = client.clone();
                        let stream_url = send_url.clone();
                        let stream_handle = handle.clone();
                        let task = tokio::spawn(async move {
                            raw_stream_loop(&stream_client, &stream_url, chat_id, &stream_handle).await;
                        });
                        state.raw_task = Some(task);
                        drop(states);

                        let sid = handle.id.clone();
                        send_message(client, &send_url, chat_id, &format!("raw mode on session {}", &sid[..8])).await;
                    }
                    "/exec" => {
                        let mut states = chat_states.lock().await;
                        let state = states.entry(chat_id).or_default();
                        state.stop_raw();
                        state.mode = Mode::Exec;
                        drop(states);

                        send_message(client, &send_url, chat_id, "exec mode").await;
                    }
                    "/sessions" => {
                        let ids = sessions.list_sessions().await;
                        let current = {
                            let states = chat_states.lock().await;
                            resolve_handle(&states)
                        };
                        let lines: Vec<String> = ids.iter().map(|id| {
                            let marker = if current.as_deref() == Some(id.as_str()) { " ◀" } else { "" };
                            format!("• {}{}", &id[..8.min(id.len())], marker)
                        }).collect();
                        let msg = if lines.is_empty() {
                            "no sessions".to_string()
                        } else {
                            lines.join("\n")
                        };
                        send_message(client, &send_url, chat_id, &msg).await;
                    }
                    "/attach" => {
                        if arg.is_empty() {
                            send_message(client, &send_url, chat_id, "usage: /attach <session_id>").await;
                            continue;
                        }
                        // Find session by prefix match.
                        let ids = sessions.list_sessions().await;
                        let matched: Vec<&String> = ids.iter().filter(|id| id.starts_with(arg)).collect();
                        match matched.len() {
                            0 => {
                                send_message(client, &send_url, chat_id, &format!("no session matching '{}'", arg)).await;
                            }
                            1 => {
                                let sid = matched[0].clone();
                                let mut states = chat_states.lock().await;
                                let state = states.entry(chat_id).or_default();
                                state.stop_raw();
                                state.mode = Mode::Exec;
                                state.session_id = Some(sid.clone());
                                drop(states);
                                send_message(client, &send_url, chat_id, &format!("attached to {}", &sid[..8])).await;
                            }
                            _ => {
                                let options: Vec<String> = matched.iter().map(|id| id[..8].to_string()).collect();
                                send_message(client, &send_url, chat_id, &format!("ambiguous, matches: {}", options.join(", "))).await;
                            }
                        }
                    }
                    "/new" => {
                        match sessions.create_session().await {
                            Ok(handle) => {
                                let sid = handle.id.clone();
                                let mut states = chat_states.lock().await;
                                let state = states.entry(chat_id).or_default();
                                state.stop_raw();
                                state.mode = Mode::Exec;
                                state.session_id = Some(sid.clone());
                                drop(states);
                                send_message(client, &send_url, chat_id, &format!("created and attached to {}", &sid[..8])).await;
                            }
                            Err(e) => {
                                send_message(client, &send_url, chat_id, &format!("failed: {}", e)).await;
                            }
                        }
                    }
                    "/detach" => {
                        let mut states = chat_states.lock().await;
                        let state = states.entry(chat_id).or_default();
                        state.stop_raw();
                        state.session_id = None;
                        state.mode = Mode::Exec;
                        drop(states);
                        send_message(client, &send_url, chat_id, "detached — using default session").await;
                    }
                    "/help" => {
                        send_message(client, &send_url, chat_id, concat!(
                            "hermytt commands:\n",
                            "/exec — command → response mode (default)\n",
                            "/raw — live stream mode\n",
                            "/sessions — list sessions\n",
                            "/attach <id> — attach to a session\n",
                            "/new — create + attach new session\n",
                            "/detach — back to default session\n",
                            "/help — this message",
                        )).await;
                    }
                    _ => {
                        send_message(client, &send_url, chat_id, "unknown command — /help").await;
                    }
                }
                continue;
            }

            // Regular input — resolve session and dispatch.
            let (session_id, mode) = {
                let states = chat_states.lock().await;
                let state = states.get(&chat_id);
                (
                    state.and_then(|s| s.session_id.clone()),
                    state.map(|s| s.mode).unwrap_or(Mode::Exec),
                )
            };

            let Some(handle) = get_handle(sessions, session_id.as_deref()).await else {
                send_message(client, &send_url, chat_id, "session not found").await;
                continue;
            };

            match mode {
                Mode::Exec => {
                    handle_exec(client, &send_url, chat_id, &handle, &text).await;
                }
                Mode::Raw => {
                    let mut input = text.into_bytes();
                    if !input.ends_with(b"\n") {
                        input.push(b'\n');
                    }
                    if let Err(e) = handle.stdin_tx.send(input).await {
                        error!(transport = "telegram", error = %e, "stdin send failed");
                    }
                }
            }
        }
    }
}

/// Resolve a session handle — by ID if specified, otherwise default.
async fn get_handle(
    sessions: &Arc<SessionManager>,
    session_id: Option<&str>,
) -> Option<hermytt_core::SessionHandle> {
    match session_id {
        Some(id) => sessions.get_session(id).await,
        None => sessions.default_session().await.ok(),
    }
}

async fn handle_exec(
    client: &reqwest::Client,
    send_url: &str,
    chat_id: i64,
    handle: &hermytt_core::SessionHandle,
    cmd: &str,
) {
    let output = match handle.execute(cmd, SILENCE_TIMEOUT).await {
        Ok(data) => data,
        Err(e) => {
            error!(transport = "telegram", error = %e, "execute failed");
            return;
        }
    };

    let raw = String::from_utf8_lossy(&output);
    let clean = clean_output(&raw, cmd);

    if clean.trim().is_empty() {
        send_message(client, send_url, chat_id, "(no output)").await;
        return;
    }

    for chunk in chunk_message(&clean) {
        let formatted = format!("```\n{}\n```", chunk);
        send_message(client, send_url, chat_id, &formatted).await;
    }
}

/// Continuously stream raw PTY output to a Telegram chat.
async fn raw_stream_loop(
    client: &reqwest::Client,
    send_url: &str,
    chat_id: i64,
    handle: &hermytt_core::SessionHandle,
) {
    let mut output = handle.subscribe_buffered(STREAM_BUFFER);

    while let Some(data) = output.recv().await {
        let raw = String::from_utf8_lossy(&data);
        let text = strip_ansi(&raw);
        if text.trim().is_empty() {
            continue;
        }

        for chunk in chunk_message(&text) {
            let formatted = format!("```\n{}\n```", chunk);
            send_message(client, send_url, chat_id, &formatted).await;
        }
    }
}

async fn send_message(client: &reqwest::Client, url: &str, chat_id: i64, text: &str) {
    let req = SendMessageRequest {
        chat_id,
        text,
        parse_mode: Some("Markdown"),
    };
    if let Err(e) = client.post(url).json(&req).send().await {
        error!(transport = "telegram", error = %e, "send failed");
    }
}

/// Clean PTY output for display: strip ANSI, remove the echoed command, remove prompt lines.
pub fn clean_output(raw: &str, _command: &str) -> String {
    let stripped = strip_ansi(raw);
    let lines: Vec<&str> = stripped.lines().collect();

    lines
        .iter()
        .skip(1) // skip echoed command
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            if matches!(trimmed, "%" | "$" | "#") {
                return false;
            }
            if trimmed.ends_with('%')
                || trimmed.ends_with('$')
                || trimmed.ends_with("% ")
                || trimmed.ends_with("$ ")
                || trimmed.ends_with("# ")
            {
                return false;
            }
            true
        })
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip ANSI escape sequences and terminal control codes.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&ch) = chars.peek() {
                    chars.next();
                    if ch.is_ascii_alphabetic() || ch == '~' || ch == '@' {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                while let Some(&ch) = chars.peek() {
                    chars.next();
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if c == '\r' {
            continue;
        } else if c.is_ascii_control() && c != '\n' && c != '\t' {
            continue;
        } else {
            out.push(c);
        }
    }

    out
}

fn chunk_message(text: &str) -> Vec<&str> {
    if text.len() <= MAX_MESSAGE_LEN {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= MAX_MESSAGE_LEN {
            chunks.push(remaining);
            break;
        }

        let split_at = remaining[..MAX_MESSAGE_LEN]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(MAX_MESSAGE_LEN);

        chunks.push(&remaining[..split_at]);
        remaining = &remaining[split_at..];
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_short_message() {
        assert_eq!(chunk_message("hello"), vec!["hello"]);
    }

    #[test]
    fn chunk_splits_on_newline() {
        let line = "x".repeat(3000);
        let text = format!("{}\n{}", line, line);
        let chunks = chunk_message(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.len() <= MAX_MESSAGE_LEN));
    }

    #[test]
    fn chunk_hard_split() {
        let text = "x".repeat(MAX_MESSAGE_LEN + 100);
        let chunks = chunk_message(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), MAX_MESSAGE_LEN);
    }

    #[test]
    fn strip_csi() {
        assert_eq!(strip_ansi("\x1b[32mhello\x1b[0m"), "hello");
    }

    #[test]
    fn strip_bracket_paste() {
        assert_eq!(strip_ansi("\x1b[?2004hwhoami\x1b[?2004l"), "whoami");
    }

    #[test]
    fn strip_cr() {
        assert_eq!(strip_ansi("hello\r\nworld"), "hello\nworld");
    }

    #[test]
    fn strip_preserves_text() {
        assert_eq!(strip_ansi("a\n\tb"), "a\n\tb");
    }

    #[test]
    fn clean_removes_echo_and_prompt() {
        let raw = "uptime\r\n 17:39  up 13 days\r\ncali@mini ~ %\r\n";
        let clean = clean_output(raw, "uptime");
        assert_eq!(clean.trim(), "17:39  up 13 days");
    }

    #[test]
    fn clean_no_output() {
        let raw = "cd /tmp\r\ncali@mini /tmp %\r\n";
        let clean = clean_output(raw, "cd /tmp");
        assert!(clean.trim().is_empty());
    }
}
