use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use hermytt_core::{BufferedOutput, RecordingHandle, SessionManager};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

use crate::Transport;
use crate::tls::TlsConfig;

pub struct RestTransport {
    pub port: u16,
    pub bind: String,
    pub auth_token: Option<String>,
    pub shell: String,
    pub transport_info: Vec<TransportInfo>,
    pub config_path: Option<String>,
    pub tls: Option<TlsConfig>,
    pub recording_dir: Option<PathBuf>,
}

#[derive(Clone, Serialize)]
pub struct TransportInfo {
    pub name: String,
    pub endpoint: String,
}

/// Active recordings keyed by session ID.
type RecordingMap = Arc<Mutex<HashMap<String, RecordingHandle>>>;

#[derive(Clone)]
struct AppState {
    sessions: Arc<SessionManager>,
    auth_token: Option<String>,
    shell: String,
    transport_info: Vec<TransportInfo>,
    config_path: Option<String>,
    recording_dir: Option<PathBuf>,
    recordings: RecordingMap,
}

#[derive(Deserialize)]
struct StdinBody {
    input: String,
}

#[derive(Serialize)]
struct SessionInfo {
    id: String,
}

#[derive(Serialize)]
struct SessionListResponse {
    sessions: Vec<SessionInfo>,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[async_trait]
impl Transport for RestTransport {
    async fn serve(self: Arc<Self>, sessions: Arc<SessionManager>) -> Result<()> {
        let state = AppState {
            sessions,
            auth_token: self.auth_token.clone(),
            shell: self.shell.clone(),
            transport_info: self.transport_info.clone(),
            config_path: self.config_path.clone(),
            recording_dir: self.recording_dir.clone(),
            recordings: Arc::new(Mutex::new(HashMap::new())),
        };

        // API routes behind auth.
        let api = Router::new()
            .route("/info", get(server_info))
            .route("/session", post(create_session))
            .route("/sessions", get(list_sessions))
            .route("/session/{id}/stdin", post(write_stdin))
            .route("/session/{id}/stdout", get(stream_stdout))
            .route("/session/{id}/record", post(start_recording))
            .route("/session/{id}/stop-record", post(stop_recording))
            .route("/recordings", get(list_recordings))
            .route("/recordings/{filename}", get(download_recording))
            .route("/stdin", post(write_stdin_default))
            .route("/stdout", get(stream_stdout_default))
            .route("/exec", post(exec_default))
            .route("/config", get(get_config))
            .route("/config", axum::routing::put(save_config))
            .route("/restart", post(restart_server))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ));

        // WebSocket: auth via first message, not query param.
        let ws_routes = Router::new()
            .route("/ws", get(ws_handler_default))
            .route("/ws/{id}", get(ws_handler));

        // Web UI from hermytt-web (public, no auth).
        let web = hermytt_web::routes();

        // TODO: lock down allow_origin to configured origins when crytter ships.
        // For now, permissive for dev. In prod, set specific origins.
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_headers(Any)
            .allow_methods(Any);

        let app = web.merge(api).merge(ws_routes).with_state(state).layer(cors);

        let addr = format!("{}:{}", self.bind, self.port);
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        info!(transport = "rest", addr = %addr, scheme = scheme, "listening");

        if let Some(ref tls) = self.tls {
            let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                &tls.cert_path,
                &tls.key_path,
            )
            .await?;
            let socket_addr: std::net::SocketAddr = addr.parse()?;
            axum_server::bind_rustls(socket_addr, rustls_config)
                .serve(app.into_make_service())
                .await?;
        } else {
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "rest"
    }
}

async fn auth_middleware(
    State(state): State<AppState>,
    Query(query): Query<TokenQuery>,
    req: Request<Body>,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    let Some(expected) = &state.auth_token else {
        // No token configured — allow all.
        return Ok(next.run(req).await);
    };

    // Check X-Hermytt-Key header first, then ?token= query param.
    let provided = req
        .headers()
        .get("X-Hermytt-Key")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or(query.token);

    match provided {
        Some(ref t) if t == expected => Ok(next.run(req).await),
        _ => {
            warn!(transport = "rest", "unauthorized request");
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

async fn server_info(State(state): State<AppState>) -> Json<serde_json::Value> {
    let sessions = state.sessions.list_sessions().await;
    Json(serde_json::json!({
        "shell": state.shell,
        "sessions": sessions.len(),
        "transports": state.transport_info,
    }))
}

async fn get_config(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let Some(path) = &state.config_path else {
        return Err(StatusCode::NOT_FOUND);
    };
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let parsed: toml::Value =
        toml::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::to_value(&parsed).unwrap_or_default()))
}

async fn save_config(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some(path) = &state.config_path else {
        return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no config file"}))));
    };

    // Validate required fields per transport.
    if let Some(transport) = body.get("transport") {
        if let Some(mqtt) = transport.get("mqtt") {
            if mqtt.get("broker").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "MQTT: broker is required"}))));
            }
        }
        if let Some(rest) = transport.get("rest") {
            if let Some(port) = rest.get("port") {
                if port.as_u64().unwrap_or(0) == 0 || port.as_u64().unwrap_or(0) > 65535 {
                    return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "REST: invalid port"}))));
                }
            }
        }
        if let Some(tcp) = transport.get("tcp") {
            if let Some(port) = tcp.get("port") {
                if port.as_u64().unwrap_or(0) == 0 || port.as_u64().unwrap_or(0) > 65535 {
                    return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "TCP: invalid port"}))));
                }
            }
        }
        if let Some(tg) = transport.get("telegram") {
            if tg.get("bot_token").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Telegram: bot_token is required"}))));
            }
        }
    }

    // Convert JSON -> TOML and write.
    let toml_value: toml::Value = serde_json::from_value(body).map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("invalid config: {}", e)})))
    })?;
    let toml_string = toml::to_string_pretty(&toml_value).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": format!("serialize failed: {}", e)})))
    })?;

    tokio::fs::write(path, &toml_string).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": format!("write failed: {}", e)})))
    })?;

    info!("config saved to {}", path);
    Ok(Json(serde_json::json!({"status": "saved", "restart_required": true})))
}

async fn restart_server() -> StatusCode {
    info!("restart requested via admin API");
    // Give the response time to flush, then exit.
    // The process supervisor (systemd, launchd, docker) restarts us.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::process::exit(0);
    });
    StatusCode::OK
}

async fn create_session(
    State(state): State<AppState>,
) -> Result<Json<SessionInfo>, StatusCode> {
    let handle = state
        .sessions
        .create_session()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(SessionInfo { id: handle.id }))
}

async fn list_sessions(State(state): State<AppState>) -> Json<SessionListResponse> {
    let ids = state.sessions.list_sessions().await;
    Json(SessionListResponse {
        sessions: ids.into_iter().map(|id| SessionInfo { id }).collect(),
    })
}

async fn send_stdin(handle: &hermytt_core::SessionHandle, input: &str) -> StatusCode {
    let mut data = input.to_string();
    if !data.ends_with('\n') {
        data.push('\n');
    }
    match handle.stdin_tx.send(data.into_bytes()).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn write_stdin(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StdinBody>,
) -> StatusCode {
    let Some(handle) = state.sessions.get_session(&id).await else {
        return StatusCode::NOT_FOUND;
    };
    send_stdin(&handle, &body.input).await
}

async fn write_stdin_default(
    State(state): State<AppState>,
    Json(body): Json<StdinBody>,
) -> StatusCode {
    let Ok(handle) = state.sessions.default_session().await else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    send_stdin(&handle, &body.input).await
}

async fn resolve_stdout(
    handle: hermytt_core::SessionHandle,
) -> impl IntoResponse {
    let output = handle.subscribe_buffered(REST_BUFFER_WINDOW);
    make_sse_stream(output)
}

async fn stream_stdout(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let Some(handle) = state.sessions.get_session(&id).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(resolve_stdout(handle).await)
}

async fn stream_stdout_default(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let Ok(handle) = state.sessions.default_session().await else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    Ok(resolve_stdout(handle).await)
}

// --- WebSocket handlers (embedded in REST for single-port mobile support) ---

async fn ws_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let Some(socket) = ws_auth(socket, &state.auth_token).await else { return };
        let Some(handle) = state.sessions.get_session(&id).await else { return };
        handle_ws_socket(socket, handle, state.sessions.clone()).await;
    })
}

async fn ws_handler_default(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let Some(socket) = ws_auth(socket, &state.auth_token).await else { return };
        let Ok(handle) = state.sessions.default_session().await else { return };
        handle_ws_socket(socket, handle, state.sessions.clone()).await;
    })
}

/// First-message auth: client sends token as first WS message.
/// Returns the socket if auth passes, None if rejected.
async fn ws_auth(mut socket: WebSocket, expected: &Option<String>) -> Option<WebSocket> {
    let Some(token) = expected else {
        // No auth configured.
        return Some(socket);
    };

    // Wait up to 5s for the first message (the token).
    let msg = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        socket.recv(),
    )
    .await;

    match msg {
        Ok(Some(Ok(Message::Text(provided)))) if provided.as_str() == token.as_str() => {
            let _ = socket.send(Message::text("auth:ok")).await;
            Some(socket)
        }
        _ => {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 4401,
                    reason: "unauthorized".into(),
                })))
                .await;
            None
        }
    }
}

async fn handle_ws_socket(
    mut socket: WebSocket,
    handle: hermytt_core::SessionHandle,
    sessions: Arc<SessionManager>,
) {
    let stdin_tx = handle.stdin_tx.clone();
    let session_id = handle.id.clone();
    let mut output_rx = handle.subscribe_output();

    loop {
        tokio::select! {
            result = output_rx.recv() => {
                match result {
                    Ok(data) if data == hermytt_core::session::PTY_EXIT_SENTINEL => {
                        // PTY exited — send exit signal to client.
                        let _ = socket.send(Message::text("{\"exit\":true}")).await;
                        break;
                    }
                    Ok(data) => {
                        let text = String::from_utf8_lossy(&data).to_string();
                        if socket.send(Message::text(text)).await.is_err() { break; }
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Intercept JSON control messages before forwarding to PTY.
                        if text.starts_with('{') {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.as_str()) {
                                if let Some(dims) = val.get("resize").and_then(|v| v.as_array()) {
                                    if let (Some(cols), Some(rows)) = (
                                        dims.first().and_then(|v| v.as_u64()),
                                        dims.get(1).and_then(|v| v.as_u64()),
                                    ) {
                                        let _ = sessions.resize_session(&session_id, cols as u16, rows as u16).await;
                                    }
                                }
                                // Any valid JSON is a control message, don't forward to PTY.
                                continue;
                            }
                            // Invalid JSON starting with { — fall through to PTY stdin.
                        }
                        if stdin_tx.send(text.as_bytes().to_vec()).await.is_err() { break; }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if stdin_tx.send(data.to_vec()).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    info!(session = %handle.id, "websocket client disconnected");
}

/// Execute a command directly (no PTY) and return clean output.

#[derive(Serialize)]
struct ExecResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

async fn exec_default(
    State(_state): State<AppState>,
    Json(body): Json<StdinBody>,
) -> Result<Json<ExecResponse>, StatusCode> {
    run_exec(&body.input).await
}

async fn run_exec(cmd: &str) -> Result<Json<ExecResponse>, StatusCode> {
    let shell = hermytt_core::platform::default_shell();
    let result = hermytt_core::exec(cmd, shell)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ExecResponse {
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
    }))
}

/// SSE buffer window — accumulate PTY output for this long before sending.
const REST_BUFFER_WINDOW: Duration = Duration::from_millis(100);

fn make_sse_stream(
    mut output: BufferedOutput,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = async_stream::stream! {
        while let Some(data) = output.recv().await {
            let text = String::from_utf8_lossy(&data).to_string();
            yield Ok(Event::default().data(text));
        }
    };
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

// --- Recording endpoints ---

async fn start_recording(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let Some(recording_dir) = &state.recording_dir else {
        return Err(StatusCode::NOT_FOUND);
    };

    let Some(handle) = state.sessions.get_session(&id).await else {
        return Err(StatusCode::NOT_FOUND);
    };

    // Check if already recording.
    {
        let recordings = state.recordings.lock().await;
        if recordings.contains_key(&id) {
            return Ok(Json(serde_json::json!({
                "error": "session is already being recorded"
            })));
        }
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = format!("{}-{}.cast", id, timestamp);
    let path = recording_dir.join(&filename);

    match hermytt_core::start_recording(&handle, &path, 80, 24, Some(format!("session-{}", id)))
        .await
    {
        Ok(rec_handle) => {
            state.recordings.lock().await.insert(id.clone(), rec_handle);
            info!(session = %id, file = %filename, "recording started via REST");
            Ok(Json(serde_json::json!({
                "status": "recording",
                "filename": filename,
            })))
        }
        Err(e) => {
            error!(error = %e, "failed to start recording");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn stop_recording(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let rec_handle = state.recordings.lock().await.remove(&id);
    let Some(rec_handle) = rec_handle else {
        return Ok(Json(serde_json::json!({
            "error": "session is not being recorded"
        })));
    };

    let filename = rec_handle
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if let Err(e) = rec_handle.stop().await {
        error!(error = %e, "failed to stop recording");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    info!(session = %id, file = %filename, "recording stopped via REST");
    Ok(Json(serde_json::json!({
        "status": "stopped",
        "filename": filename,
    })))
}

async fn list_recordings(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let Some(recording_dir) = &state.recording_dir else {
        return Ok(Json(serde_json::json!({ "recordings": [] })));
    };

    let entries = hermytt_core::list_recordings(recording_dir)
        .await
        .map_err(|e| {
            error!(error = %e, "failed to list recordings");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(serde_json::json!({ "recordings": entries })))
}

async fn download_recording(
    State(state): State<AppState>,
    Path(filename): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let Some(recording_dir) = &state.recording_dir else {
        return Err(StatusCode::NOT_FOUND);
    };

    // Sanitize filename to prevent path traversal.
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return Err(StatusCode::BAD_REQUEST);
    }

    let path = recording_dir.join(&filename);
    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let data = tokio::fs::read(&path).await.map_err(|e| {
        error!(error = %e, "failed to read recording file");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let disposition = format!("attachment; filename=\"{}\"", filename);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/x-asciicast".to_string(),
            ),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        data,
    ))
}

/// Start auto-recording for a session. Called from server startup when auto_record is enabled.
pub async fn auto_record_session(
    handle: &hermytt_core::SessionHandle,
    recording_dir: &std::path::Path,
    recordings: &RecordingMap,
) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = format!("{}-{}.cast", handle.id, timestamp);
    let path = recording_dir.join(&filename);

    match hermytt_core::start_recording(handle, &path, 80, 24, Some(format!("session-{}", handle.id)))
        .await
    {
        Ok(rec_handle) => {
            recordings.lock().await.insert(handle.id.clone(), rec_handle);
            info!(session = %handle.id, file = %filename, "auto-recording started");
        }
        Err(e) => {
            error!(session = %handle.id, error = %e, "failed to auto-record session");
        }
    }
}

