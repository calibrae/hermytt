use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use hermytt_core::control::{ControlMessage, ShyttiMessage};
use hermytt_core::{ServiceRegistry, SessionManager};
use serde::Deserialize;
use tracing::{info, warn};

use crate::rest::AppState;

// --- Control WS (shytti → hermytt persistent control channel, Mode 1) ---

pub(crate) async fn control_ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        handle_control_ws(socket, state).await;
    })
}

async fn handle_control_ws(mut socket: WebSocket, state: AppState) {
    // First message: auth + identity.
    let (name, _role) = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        socket.recv(),
    ).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            let v: serde_json::Value = match serde_json::from_str(text.as_str()) {
                Ok(v) => v,
                Err(_) => return,
            };

            // Auth check.
            if let Some(expected) = &state.auth_token {
                let token = v.get("auth").and_then(|t| t.as_str()).unwrap_or("");
                if token != expected {
                    warn!(transport = "rest", "unauthorized control WS");
                    let _ = socket.send(Message::text(r#"{"error":"unauthorized"}"#)).await;
                    return;
                }
            }

            let name = v.get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();
            let role = v.get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("shell")
                .to_string();

            // Send auth ok.
            let _ = socket.send(Message::text(r#"{"status":"ok"}"#)).await;

            (name, role)
        }
        _ => return,
    };

    // Register in control hub.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlMessage>(64);
    state.control_hub.register(name.clone(), serde_json::Value::Null, tx).await;

    // Register in service registry.
    let svc_info = hermytt_core::registry::ServiceInfo {
        name: name.clone(),
        role: hermytt_core::registry::ServiceRole::Shell,
        endpoint: "control-ws".to_string(),
        status: hermytt_core::registry::ServiceStatus::Connected,
        last_seen_instant: None,
        last_seen: 0,
        meta: serde_json::Value::Null,
    };
    state.registry.register(svc_info).await;

    info!(name = %name, "control WS established");

    let mut shells_requested = false;

    loop {
        tokio::select! {
            // Messages from shytti.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(shytti_msg) = serde_json::from_str::<ShyttiMessage>(text.as_str()) {
                            match shytti_msg {
                                ShyttiMessage::Heartbeat { meta } => {
                                    state.control_hub.heartbeat(&name, meta.clone()).await;
                                    // Update registry too.
                                    let svc = hermytt_core::registry::ServiceInfo {
                                        name: name.clone(),
                                        role: hermytt_core::registry::ServiceRole::Shell,
                                        endpoint: "control-ws".to_string(),
                                        status: hermytt_core::registry::ServiceStatus::Connected,
                                        last_seen_instant: None,
                                        last_seen: 0,
                                        meta,
                                    };
                                    state.registry.register(svc).await;
                                    // Request shell list after first heartbeat (session recovery).
                                    if !shells_requested {
                                        shells_requested = true;
                                        let msg = serde_json::to_string(&ControlMessage::ListShells {}).unwrap_or_default();
                                        let _ = socket.send(Message::text(msg)).await;
                                    }
                                }
                                ShyttiMessage::SpawnOk { req_id, shell_id, session_id } => {
                                    // Register the managed session so it appears in /sessions.
                                    let _ = state.sessions.register_session(Some(session_id.clone()), Some(name.clone())).await;
                                    info!(name = %name, session = %session_id, "spawn ok — session registered");

                                    // Spawn stdin forwarder: session stdin → control WS Input messages.
                                    if let Some(mut stdin_rx) = state.sessions.take_stdin_rx(&session_id).await {
                                        let hub = state.control_hub.clone();
                                        let host_name = name.clone();
                                        let sid = session_id.clone();
                                        tokio::spawn(async move {
                                            use base64::Engine;
                                            while let Some(data) = stdin_rx.recv().await {
                                                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                                                let msg = ControlMessage::Input {
                                                    session_id: sid.clone(),
                                                    data: b64,
                                                };
                                                if hub.send_to(&host_name, msg).await.is_err() { break; }
                                            }
                                        });
                                    }

                                    state.control_hub.handle_spawn_ok(&req_id, shell_id, session_id).await;
                                }
                                ShyttiMessage::SpawnErr { req_id, error } => {
                                    warn!(name = %name, error = %error, "spawn failed");
                                    state.control_hub.handle_spawn_err(&req_id, error).await;
                                }
                                ShyttiMessage::KillOk { shell_id } => {
                                    info!(name = %name, shell = %shell_id, "kill ok");
                                }
                                ShyttiMessage::ShellDied { shell_id, session_id } => {
                                    info!(name = %name, shell = %shell_id, "shell died");
                                    // Send exit sentinel to the session so browsers get {"exit":true}.
                                    if let Some(sid) = session_id {
                                        if let Some(handle) = state.sessions.get_session(&sid).await {
                                            let _ = handle.output_tx.send(
                                                hermytt_core::session::PTY_EXIT_SENTINEL.to_vec()
                                            );
                                        }
                                        // Clean up the managed session.
                                        let _ = state.sessions.unregister_session(&sid).await;
                                    }
                                }
                                ShyttiMessage::Data { session_id, data } => {
                                    // PTY output from shytti → broadcast to transports.
                                    if let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &data) {
                                        if let Some(handle) = state.sessions.get_session(&session_id).await {
                                            let _ = handle.output_tx.send(bytes);
                                        }
                                    }
                                }
                                ShyttiMessage::ShellsList { shells } => {
                                    info!(name = %name, count = shells.len(), "recovering shells");
                                    for shell in shells {
                                        if state.sessions.get_session(&shell.session_id).await.is_none() {
                                            let _ = state.sessions.register_session(Some(shell.session_id.clone()), Some(name.clone())).await;
                                            info!(name = %name, session = %shell.session_id, "session recovered");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            // Messages to shytti (from hermytt).
            cmd = rx.recv() => {
                match cmd {
                    Some(msg) => {
                        let json = serde_json::to_string(&msg).unwrap_or_default();
                        if socket.send(Message::text(json)).await.is_err() { break; }
                    }
                    None => break,
                }
            }
        }
    }

    // Cleanup.
    state.control_hub.unregister(&name).await;
    state.registry.unregister(&name).await;
    info!(name = %name, "control WS closed");
}

// --- Host management ---

pub(crate) async fn list_hosts(State(state): State<AppState>) -> Json<serde_json::Value> {
    let hosts = state.control_hub.list().await;
    Json(serde_json::json!({
        "hosts": hosts.iter().map(|(name, meta)| serde_json::json!({
            "name": name,
            "meta": meta,
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
pub(crate) struct SpawnOnHostBody {
    shell: Option<String>,
    cwd: Option<String>,
}

pub(crate) async fn spawn_on_host(
    State(state): State<AppState>,
    Path(host): Path<String>,
    body: Option<Json<SpawnOnHostBody>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (shell, cwd) = body.map(|b| (b.shell.clone(), b.cwd.clone())).unwrap_or((None, None));

    match state.control_hub.spawn(&host, shell, cwd).await {
        Ok(result) => Ok(Json(serde_json::json!({
            "session_id": result.session_id,
            "shell_id": result.shell_id,
            "host": host,
        }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e})))),
    }
}

// --- Mode 2: Pairing (hermytt connects out to shytti) ---

#[derive(Deserialize)]
pub(crate) struct PairHostBody {
    token: String,
}

pub(crate) async fn pair_host(
    State(state): State<AppState>,
    Json(body): Json<PairHostBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use hermytt_core::pairing::PairingToken;

    // Decode token.
    let token = PairingToken::decode(&body.token).map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("invalid token: {}", e)})))
    })?;

    let pair_url = token.ws_url();
    info!(url = %pair_url, "pairing with shytti...");

    // Connect to shytti's /pair WS.
    let (mut ws, _) = tokio_tungstenite::connect_async(&pair_url).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("connection failed: {}", e)})))
    })?;

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as TsMessage;

    // Send pairing key.
    let pair_msg = serde_json::json!({"pair_key": token.key});
    ws.send(TsMessage::Text(pair_msg.to_string().into())).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("send failed: {}", e)})))
    })?;

    // Wait for response with long-lived key.
    let resp = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next()).await
        .map_err(|_| (StatusCode::GATEWAY_TIMEOUT, Json(serde_json::json!({"error": "pairing timed out"}))))?
        .ok_or_else(|| (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": "connection closed"}))))?
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("ws error: {}", e)}))))?;

    let resp_text = match resp {
        TsMessage::Text(t) => t.to_string(),
        _ => return Err((StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": "unexpected response"})))),
    };

    let resp_val: serde_json::Value = serde_json::from_str(&resp_text).map_err(|_| {
        (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": "invalid response"})))
    })?;

    let long_lived_key = resp_val.get("long_lived_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": "no long_lived_key in response"}))))?
        .to_string();

    let host_name = resp_val.get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(&format!("shytti-{}:{}", token.ip, token.port))
        .to_string();

    // Store the key.
    {
        let mut hosts = state.paired_hosts.lock().await;
        hosts.add(
            hermytt_core::pairing::PairedHost {
                name: host_name.clone(),
                ip: token.ip.clone(),
                port: token.port,
                key: long_lived_key.clone(),
            },
            &state.keys_path,
        );
    }

    info!(name = %host_name, ip = %token.ip, "pairing successful");

    // Now this WS becomes the control channel. Spawn the control loop.
    let control_hub = state.control_hub.clone();
    let registry = state.registry.clone();
    let sess = state.sessions.clone();
    let name = host_name.clone();

    tokio::spawn(async move {
        run_outbound_control(ws, name, control_hub, registry, sess).await;
    });

    Ok(Json(serde_json::json!({
        "status": "paired",
        "name": host_name,
        "ip": token.ip,
        "port": token.port,
    })))
}

/// Run the control protocol on an outbound WS (Mode 2).
/// Same protocol as handle_control_ws but we're the client.
async fn run_outbound_control(
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    name: String,
    control_hub: Arc<hermytt_core::ControlHub>,
    registry: Arc<ServiceRegistry>,
    sessions: Arc<SessionManager>,
) {
    use futures_util::{SinkExt, StreamExt};
    use hermytt_core::control::ControlMessage;
    use tokio_tungstenite::tungstenite::Message as TsMessage;

    let (mut ws_tx, mut ws_rx) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ControlMessage>(64);

    control_hub.register(name.clone(), serde_json::Value::Null, tx).await;

    let svc = hermytt_core::registry::ServiceInfo {
        name: name.clone(),
        role: hermytt_core::registry::ServiceRole::Shell,
        endpoint: "paired".to_string(),
        status: hermytt_core::registry::ServiceStatus::Connected,
        last_seen_instant: None,
        last_seen: 0,
        meta: serde_json::Value::Null,
    };
    registry.register(svc).await;

    info!(name = %name, "outbound control channel active");

    let mut shells_requested = false;

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(TsMessage::Text(text))) => {
                        match serde_json::from_str::<ShyttiMessage>(&text) {
                            Ok(shytti_msg) => { match shytti_msg {
                                ShyttiMessage::Heartbeat { meta } => {
                                    control_hub.heartbeat(&name, meta.clone()).await;
                                    let svc = hermytt_core::registry::ServiceInfo {
                                        name: name.clone(),
                                        role: hermytt_core::registry::ServiceRole::Shell,
                                        endpoint: "paired".to_string(),
                                        status: hermytt_core::registry::ServiceStatus::Connected,
                                        last_seen_instant: None,
                                        last_seen: 0,
                                        meta,
                                    };
                                    registry.register(svc).await;
                                    // Request shell list after first heartbeat (session recovery).
                                    if !shells_requested {
                                        shells_requested = true;
                                        let msg = serde_json::to_string(&ControlMessage::ListShells {}).unwrap_or_default();
                                        let _ = ws_tx.send(TsMessage::text(msg)).await;
                                    }
                                }
                                ShyttiMessage::SpawnOk { req_id, shell_id, session_id } => {
                                    let _ = sessions.register_session(Some(session_id.clone()), Some(name.clone())).await;
                                    info!(name = %name, session = %session_id, "spawn ok — session registered");

                                    // Stdin forwarder: session stdin → control WS Input.
                                    if let Some(mut stdin_rx) = sessions.take_stdin_rx(&session_id).await {
                                        let hub = control_hub.clone();
                                        let host_name = name.clone();
                                        let sid = session_id.clone();
                                        tokio::spawn(async move {
                                            use base64::Engine;
                                            while let Some(data) = stdin_rx.recv().await {
                                                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                                                let msg = hermytt_core::control::ControlMessage::Input {
                                                    session_id: sid.clone(),
                                                    data: b64,
                                                };
                                                if hub.send_to(&host_name, msg).await.is_err() { break; }
                                            }
                                        });
                                    }

                                    control_hub.handle_spawn_ok(&req_id, shell_id, session_id).await;
                                }
                                ShyttiMessage::SpawnErr { req_id, error } => {
                                    control_hub.handle_spawn_err(&req_id, error).await;
                                }
                                ShyttiMessage::KillOk { .. } => {}
                                ShyttiMessage::ShellDied { shell_id: _, session_id } => {
                                    if let Some(sid) = session_id {
                                        if let Some(handle) = sessions.get_session(&sid).await {
                                            let _ = handle.output_tx.send(hermytt_core::session::PTY_EXIT_SENTINEL.to_vec());
                                        }
                                        let _ = sessions.unregister_session(&sid).await;
                                    }
                                }
                                ShyttiMessage::Data { session_id, data } => {
                                    if let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &data) {
                                        if let Some(handle) = sessions.get_session(&session_id).await {
                                            let _ = handle.output_tx.send(bytes);
                                        }
                                    }
                                }
                                ShyttiMessage::ShellsList { shells } => {
                                    info!(name = %name, count = shells.len(), "recovering shells");
                                    for shell in shells {
                                        if sessions.get_session(&shell.session_id).await.is_none() {
                                            let _ = sessions.register_session(Some(shell.session_id.clone()), Some(name.clone())).await;
                                            info!(name = %name, session = %shell.session_id, "session recovered");
                                        }
                                    }
                                }
                            } }
                            Err(e) => {
                                warn!(name = %name, error = %e, "outbound: unrecognized message");
                            }
                        }
                    }
                    Some(Ok(TsMessage::Close(_))) | None => break,
                    _ => {}
                }
            }
            cmd = rx.recv() => {
                match cmd {
                    Some(msg) => {
                        let json = serde_json::to_string(&msg).unwrap_or_default();
                        if ws_tx.send(TsMessage::Text(json.into())).await.is_err() { break; }
                    }
                    None => break,
                }
            }
        }
    }

    control_hub.unregister(&name).await;
    registry.unregister(&name).await;
    info!(name = %name, "outbound control channel closed");
}

/// Connect to a Mode 2 paired host using the long-lived key.
/// Reconnects with exponential backoff on disconnect.
pub async fn connect_paired_host(
    host: hermytt_core::pairing::PairedHost,
    _auth_token: Option<String>,
    control_hub: Arc<hermytt_core::ControlHub>,
    registry: Arc<ServiceRegistry>,
    sessions: Arc<SessionManager>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as TsMessage;

    let mut backoff = 1u64;
    let max_backoff = 30u64;

    loop {
        let control_url = format!("ws://{}:{}/control", host.ip, host.port);
        info!(name = %host.name, url = %control_url, "connecting to paired host...");

        let ws = match tokio_tungstenite::connect_async(&control_url).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                warn!(name = %host.name, error = %e, backoff = backoff, "connection failed, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        let (mut ws_tx, ws_rx) = ws.split();

        // Authenticate with long-lived key.
        let auth_msg = serde_json::json!({
            "type": "auth",
            "auth": host.key,
            "name": "hermytt",
            "role": "controller"
        });
        if ws_tx.send(TsMessage::Text(auth_msg.to_string().into())).await.is_err() {
            warn!(name = %host.name, "auth send failed");
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(max_backoff);
            continue;
        }

        // Read and discard auth response before entering control loop.
        let mut ws_rx = ws_rx;
        match ws_rx.next().await {
            Some(Ok(TsMessage::Text(resp))) => {
                tracing::debug!(name = %host.name, resp = %resp, "auth response");
            }
            Some(Ok(TsMessage::Close(_))) | None => {
                warn!(name = %host.name, "connection closed during auth");
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
            _ => {}
        }

        // Reassemble for the control loop.
        let ws = ws_rx.reunite(ws_tx).unwrap();

        info!(name = %host.name, "paired host connected");
        backoff = 1; // reset on success

        run_outbound_control(ws, host.name.clone(), control_hub.clone(), registry.clone(), sessions.clone()).await;

        warn!(name = %host.name, "paired host disconnected, reconnecting in {}s", backoff);
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Spawn reconnect loops for all paired hosts from the keys file.
pub fn spawn_paired_host_connections(
    keys_path: &std::path::Path,
    auth_token: Option<String>,
    control_hub: Arc<hermytt_core::ControlHub>,
    registry: Arc<ServiceRegistry>,
    sessions: Arc<SessionManager>,
) {
    let hosts = hermytt_core::pairing::PairedHosts::load(keys_path);
    for (name, host) in hosts.hosts {
        info!(name = %name, ip = %host.ip, "spawning reconnect loop for paired host");
        let auth = auth_token.clone();
        let hub = control_hub.clone();
        let reg = registry.clone();
        let sess = sessions.clone();
        tokio::spawn(async move {
            connect_paired_host(host, auth, hub, reg, sess).await;
        });
    }
}
