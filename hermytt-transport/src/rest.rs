use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::{Multipart, Path, Query, State, WebSocketUpgrade};
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use hermytt_core::{BufferedOutput, RecordingHandle, ServiceRegistry, SessionManager};
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
    pub files_dir: Option<PathBuf>,
    pub max_upload_size: usize,
    /// Optional extra routes merged into the app (e.g., web UI). Must be stateless.
    pub extra_routes: Option<Router>,
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
    files_dir: Option<PathBuf>,
    max_upload_size: usize,
    registry: Arc<ServiceRegistry>,
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
            files_dir: self.files_dir.clone(),
            max_upload_size: self.max_upload_size,
            registry: Arc::new(ServiceRegistry::new()),
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
            .route("/files", get(list_files))
            .route("/files/upload", post(upload_file))
            .route("/files/{filename}", get(download_file))
            .route("/files/{filename}", axum::routing::delete(delete_file))
            // Service registry.
            .route("/registry", get(registry_list))
            .route("/registry/announce", post(registry_announce))
            .route("/registry/{name}", axum::routing::delete(registry_unregister))
            // Bootstrap scripts for family members.
            .route("/bootstrap/shytti", get(bootstrap_shytti))
            // Internal session API (for Shytti and external orchestrators).
            .route("/internal/session", post(register_managed_session))
            .route("/internal/session/{id}", axum::routing::delete(unregister_managed_session))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ));

        // WebSocket: auth via first message, not query param.
        let ws_routes = Router::new()
            .route("/ws", get(ws_handler_default))
            .route("/ws/{id}", get(ws_handler))
            .route("/internal/session/{id}/pipe", get(internal_pipe_handler));

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_headers(Any)
            .allow_methods(Any);

        let mut app = api.merge(ws_routes).with_state(state).layer(cors);

        // Merge optional extra routes (e.g., web UI).
        if let Some(extra) = &self.extra_routes {
            app = extra.clone().merge(app);
        }

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
    let services = state.registry.list().await;
    Json(serde_json::json!({
        "shell": state.shell,
        "sessions": sessions.len(),
        "transports": state.transport_info,
        "services": services,
    }))
}

// --- Service registry endpoints ---

#[derive(Deserialize)]
struct AnnounceBody {
    name: String,
    role: hermytt_core::registry::ServiceRole,
    endpoint: String,
    #[serde(default)]
    meta: serde_json::Value,
}

async fn registry_announce(
    State(state): State<AppState>,
    Json(body): Json<AnnounceBody>,
) -> Json<serde_json::Value> {
    let info = hermytt_core::registry::ServiceInfo {
        name: body.name.clone(),
        role: body.role,
        endpoint: body.endpoint,
        status: hermytt_core::registry::ServiceStatus::Connected,
        last_seen_instant: None,
        last_seen: 0,
        meta: body.meta,
    };
    state.registry.register(info).await;
    Json(serde_json::json!({"ok": true, "name": body.name}))
}

async fn registry_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let services = state.registry.list().await;
    Json(serde_json::json!({"services": services}))
}

async fn registry_unregister(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if state.registry.unregister(&name).await {
        Ok(Json(serde_json::json!({"ok": true})))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

// --- Config endpoints ---

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
        handle_ws_socket(socket, handle, state.sessions.clone(), state.files_dir.clone()).await;
    })
}

async fn ws_handler_default(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let Some(socket) = ws_auth(socket, &state.auth_token).await else { return };
        let Ok(handle) = state.sessions.default_session().await else { return };
        handle_ws_socket(socket, handle, state.sessions.clone(), state.files_dir.clone()).await;
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
    files_dir: Option<PathBuf>,
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
                                // Handle pasted images.
                                if let Some(paste) = val.get("paste_image") {
                                    if let (Some(dir), Some(data_str)) = (&files_dir, paste.get("data").and_then(|v| v.as_str())) {
                                        if let Ok(data) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_str) {
                                            let ts = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_secs();
                                            let ext = paste.get("name")
                                                .and_then(|n| n.as_str())
                                                .and_then(|n| n.rsplit('.').next())
                                                .unwrap_or("png");
                                            let filename = format!("paste-{}.{}", ts, ext);
                                            let path = dir.join(&filename);
                                            let _ = tokio::fs::create_dir_all(dir).await;
                                            if tokio::fs::write(&path, &data).await.is_ok() {
                                                info!(file = %filename, size = data.len(), "image pasted via WS");
                                                let _ = socket.send(Message::text(
                                                    serde_json::json!({"image_saved": filename}).to_string()
                                                )).await;
                                            }
                                        }
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

// --- File transfer endpoints ---

/// Reject filenames with path traversal or directory separators.
fn sanitize_filename(name: &str) -> Result<&str, StatusCode> {
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(name)
}

async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some(files_dir) = &state.files_dir else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "file transfer not configured"})),
        ));
    };

    // Ensure the directory exists.
    tokio::fs::create_dir_all(files_dir).await.map_err(|e| {
        error!(error = %e, "failed to create files directory");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "server error"})),
        )
    })?;

    let mut saved = Vec::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        let filename = field
            .file_name()
            .unwrap_or("upload")
            .to_string();

        let filename = sanitize_filename(&filename).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid filename: {}", filename)})),
            )
        })?;
        let filename = filename.to_string();

        let data = field.bytes().await.map_err(|e| {
            error!(error = %e, "failed to read upload");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "failed to read upload data"})),
            )
        })?;

        if data.len() > state.max_upload_size {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "error": format!("file too large: {} bytes (max {})", data.len(), state.max_upload_size)
                })),
            ));
        }

        let path = files_dir.join(&filename);
        tokio::fs::write(&path, &data).await.map_err(|e| {
            error!(error = %e, "failed to write file");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "failed to save file"})),
            )
        })?;

        info!(file = %filename, size = data.len(), "file uploaded");
        saved.push(serde_json::json!({
            "filename": filename,
            "size": data.len(),
        }));
    }

    Ok(Json(serde_json::json!({ "uploaded": saved })))
}

async fn download_file(
    State(state): State<AppState>,
    Path(filename): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let Some(files_dir) = &state.files_dir else {
        return Err(StatusCode::NOT_FOUND);
    };

    let filename = sanitize_filename(&filename)?;
    let path = files_dir.join(filename);

    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let data = tokio::fs::read(&path).await.map_err(|e| {
        error!(error = %e, "failed to read file");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let disposition = format!("attachment; filename=\"{}\"", filename);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        data,
    ))
}

async fn list_files(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let Some(files_dir) = &state.files_dir else {
        return Ok(Json(serde_json::json!({ "files": [] })));
    };

    if !files_dir.exists() {
        return Ok(Json(serde_json::json!({ "files": [] })));
    }

    let mut entries = Vec::new();
    let mut dir = tokio::fs::read_dir(files_dir).await.map_err(|e| {
        error!(error = %e, "failed to list files");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    while let Ok(Some(entry)) = dir.next_entry().await {
        let meta = entry.metadata().await.ok();
        if let Some(m) = &meta {
            if !m.is_file() {
                continue;
            }
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let size = meta.map(|m| m.len()).unwrap_or(0);
        entries.push(serde_json::json!({
            "filename": name,
            "size": size,
        }));
    }

    Ok(Json(serde_json::json!({ "files": entries })))
}

async fn delete_file(
    State(state): State<AppState>,
    Path(filename): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let Some(files_dir) = &state.files_dir else {
        return Err(StatusCode::NOT_FOUND);
    };

    let filename = sanitize_filename(&filename)?;
    let path = files_dir.join(filename);

    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    tokio::fs::remove_file(&path).await.map_err(|e| {
        error!(error = %e, "failed to delete file");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    info!(file = %filename, "file deleted");
    Ok(Json(serde_json::json!({ "status": "deleted", "filename": filename })))
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

// --- Bootstrap scripts ---

async fn bootstrap_shytti(
    State(state): State<AppState>,
    req: Request<Body>,
) -> impl IntoResponse {
    let host = req.headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:7777");
    let scheme = if state.config_path.is_some() { "http" } else { "http" }; // TODO: detect TLS
    let hermytt_url = format!("{}://{}", scheme, host);
    let token = state.auth_token.clone().unwrap_or_default();

    let script = format!(r#"#!/bin/bash
set -e

# hermytt bootstrap: install shytti on this machine
# generated by {hermytt_url}

HERMYTT_URL="{hermytt_url}"
HERMYTT_KEY="{token}"
INSTALL_DIR="/opt/shytti"
SERVICE_NAME="shytti"

echo "=== hermytt bootstrap: shytti ==="
echo "hermytt: $HERMYTT_URL"

# Detect platform
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) echo "unsupported arch: $ARCH"; exit 1 ;;
esac

echo "platform: $OS/$ARCH"

# Download shytti binary
DOWNLOAD_URL="https://github.com/calibrae/shytti/releases/latest/download/shytti-$OS-$ARCH"
echo "downloading shytti from $DOWNLOAD_URL..."
sudo mkdir -p "$INSTALL_DIR"
sudo curl -fSL "$DOWNLOAD_URL" -o "$INSTALL_DIR/shytti" || {{
    echo "download failed — you may need to build shytti manually"
    echo "  git clone https://github.com/calibrae/shytti"
    echo "  cd shytti && cargo build --release"
    echo "  sudo cp target/release/shytti $INSTALL_DIR/"
    exit 1
}}
sudo chmod +x "$INSTALL_DIR/shytti"

# Write config
echo "writing config..."
sudo tee "$INSTALL_DIR/shytti.toml" > /dev/null << CONF
[hermytt]
url = "$HERMYTT_URL"
token = "$HERMYTT_KEY"

[shell]
default = "${{SHELL:-/bin/bash}}"
CONF

# Install systemd service
if command -v systemctl &> /dev/null; then
    echo "installing systemd service..."
    sudo tee /etc/systemd/system/$SERVICE_NAME.service > /dev/null << SVC
[Unit]
Description=shytti — shell orchestrator for hermytt
After=network.target

[Service]
Type=simple
User=$(whoami)
ExecStart=$INSTALL_DIR/shytti daemon -c $INSTALL_DIR/shytti.toml
Restart=always
RestartSec=2
Environment=HERMYTT_URL=$HERMYTT_URL
Environment=HERMYTT_KEY=$HERMYTT_KEY

[Install]
WantedBy=multi-user.target
SVC

    sudo systemctl daemon-reload
    sudo systemctl enable "$SERVICE_NAME"
    sudo systemctl start "$SERVICE_NAME"
    sleep 2
    if systemctl is-active --quiet "$SERVICE_NAME"; then
        echo ""
        echo "=== shytti installed and running ==="
        echo "service: sudo systemctl status $SERVICE_NAME"
        echo "logs:    sudo journalctl -u $SERVICE_NAME -f"
        echo "config:  $INSTALL_DIR/shytti.toml"
    else
        echo "warning: service started but may have issues"
        echo "check:   sudo journalctl -u $SERVICE_NAME -e"
    fi
else
    echo ""
    echo "=== shytti installed (no systemd) ==="
    echo "run manually: $INSTALL_DIR/shytti serve -c $INSTALL_DIR/shytti.toml"
fi
"#, hermytt_url = hermytt_url, token = token);

    (
        [(header::CONTENT_TYPE, "text/x-shellscript")],
        script,
    )
}

// --- Internal session API (for Shytti and external orchestrators) ---

#[derive(Deserialize)]
struct RegisterSessionBody {
    id: Option<String>,
}

async fn register_managed_session(
    State(state): State<AppState>,
    body: Option<Json<RegisterSessionBody>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let id = body.and_then(|b| b.id.clone());
    let handle = state
        .sessions
        .register_session(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "id": handle.id,
        "pipe": format!("/internal/session/{}/pipe", handle.id),
    })))
}

async fn unregister_managed_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    state
        .sessions
        .unregister_session(&id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({"status": "unregistered", "id": id})))
}

/// Internal WS pipe: bidirectional session bridge for Shytti.
/// Shytti sends PTY stdout into this socket, receives stdin from it.
/// Resize control messages are forwarded to connected transports.
async fn internal_pipe_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let Some(socket) = ws_auth(socket, &state.auth_token).await else { return };
        let Some(handle) = state.sessions.get_session(&id).await else { return };
        handle_internal_pipe(socket, handle, state.sessions.clone()).await;
    })
}

/// Bidirectional pipe for Shytti:
/// - Shytti sends PTY output → we broadcast to all transports
/// - Transport stdin → we forward to Shytti
async fn handle_internal_pipe(
    mut socket: WebSocket,
    handle: hermytt_core::SessionHandle,
    sessions: Arc<SessionManager>,
) {
    let output_tx = handle.output_tx.clone();
    let session_id = handle.id.clone();

    // Take the stdin receiver — only works for managed sessions.
    let stdin_rx = sessions.take_stdin_rx(&session_id).await;

    if let Some(mut stdin_rx) = stdin_rx {
        // Full bidirectional pipe.
        loop {
            tokio::select! {
                // Transport stdin → send to Shytti.
                data = stdin_rx.recv() => {
                    match data {
                        Some(bytes) => {
                            if socket.send(Message::Binary(bytes.into())).await.is_err() { break; }
                        }
                        None => break,
                    }
                }
                // Shytti output → broadcast to transports.
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            let _ = output_tx.send(text.as_bytes().to_vec());
                        }
                        Some(Ok(Message::Binary(data))) => {
                            let _ = output_tx.send(data.to_vec());
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        _ => {}
                    }
                }
            }
        }
    } else {
        // No stdin_rx — output-only pipe (PTY session or already taken).
        loop {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => {
                    let _ = output_tx.send(text.as_bytes().to_vec());
                }
                Some(Ok(Message::Binary(data))) => {
                    let _ = output_tx.send(data.to_vec());
                }
                Some(Ok(Message::Close(_))) | None => break,
                _ => {}
            }
        }
    }

    info!(session = %session_id, "internal pipe disconnected");
}

