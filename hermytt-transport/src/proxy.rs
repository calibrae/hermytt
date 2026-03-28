use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use crate::rest::AppState;

pub(crate) async fn registry_proxy(
    State(state): State<AppState>,
    Path((name, path)): Path<(String, String)>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.registry.get(&name).await.ok_or_else(|| {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": format!("service '{}' not found", name)})))
    })?;
    if svc.status == hermytt_core::registry::ServiceStatus::Disconnected {
        return Err((StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("service '{}' is disconnected", name)}))));
    }
    if !svc.endpoint.starts_with("http") {
        return Err((StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("service '{}' has no HTTP endpoint", name)}))));
    }

    let path = path.trim_start_matches('/');
    let url = format!("{}/{}", svc.endpoint.trim_end_matches('/'), path);

    let client = reqwest::Client::new();
    let mut req = client.request(method.clone(), &url);
    if let Some(ct) = headers.get("content-type") {
        req = req.header("content-type", ct);
    }
    if !body.is_empty() {
        req = req.body(body.to_vec());
    }

    let resp = req.send().await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": format!("proxy error: {}", e)})))
    })?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let resp_body = resp.bytes().await.unwrap_or_default();

    Ok(axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(resp_body))
        .unwrap())
}
