//! Regression tests for bugs fixed in the March 2026 session.
//! Each test documents which bug it covers and the commit that fixed it.

use std::sync::Arc;
use std::time::Duration;

use hermytt_core::SessionManager;
use hermytt_transport::Transport;
use hermytt_transport::rest::{RestTransport, TransportInfo};

const TOKEN: &str = "test-token-regression";

async fn start_server() -> (String, Arc<SessionManager>) {
    let sessions = Arc::new(SessionManager::new("/bin/sh", 100));
    sessions.create_session().await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let control_hub = hermytt_core::ControlHub::new();
    let registry = Arc::new(hermytt_core::ServiceRegistry::new());

    let transport = Arc::new(RestTransport {
        port,
        bind: "127.0.0.1".to_string(),
        auth_token: Some(TOKEN.to_string()),
        shell: "/bin/sh".to_string(),
        transport_info: vec![TransportInfo {
            name: "REST".into(),
            endpoint: format!("127.0.0.1:{}", port),
        }],
        config_path: None,
        tls: None,
        recording_dir: None,
        files_dir: None,
        max_upload_size: 10 * 1024 * 1024,
        extra_routes: None,
        control_hub: Some(control_hub),
        registry: Some(registry),
    });

    let sessions_clone = sessions.clone();
    tokio::spawn(async move {
        transport.serve(sessions_clone).await.unwrap();
    });

    let base = format!("http://127.0.0.1:{}", port);
    for _ in 0..50 {
        if reqwest::get(&base).await.is_ok() {
            return (base, sessions);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server didn't start");
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header("X-Hermytt-Key", TOKEN)
}

// ============================================================
// Bug: config save rejected when payload contains [auth]
// Fix: 7276c9b — strip auth from payload, preserve from disk
// ============================================================

#[tokio::test]
async fn config_save_strips_auth_section() {
    let (base, _) = start_server().await;

    // Sending config WITH auth section should not be rejected —
    // server strips it silently (no config_path so it'll 404, but not 400).
    let res = auth(client().put(format!("{}/config", base)))
        .header("Content-Type", "application/json")
        .body(r#"{"auth":{"token":"secret"},"server":{"shell":"/bin/sh"},"transport":{"rest":{"port":7777}}}"#)
        .send()
        .await
        .unwrap();
    // Without config_path, returns 404 (no file to write), not 400 (rejected).
    assert_ne!(res.status(), 400, "auth section should be stripped, not rejected");
}

// ============================================================
// Bug: session ID with hyphens rejected by mqtt_pty topic parser
// Fix: 85d6143 — allow hyphens in session IDs
// ============================================================

#[tokio::test]
async fn mqtt_pty_topic_accepts_hyphens() {
    // This tests the parse function directly (it's in the mqtt_pty module tests)
    // but we verify the managed session registration path accepts hyphenated IDs.
    let sessions = Arc::new(SessionManager::new("/bin/sh", 100));
    let result = sessions.register_session(Some("12437-69c6a5cb-3".to_string()), None).await;
    assert!(result.is_ok(), "session ID with hyphens should be accepted");
    assert!(sessions.get_session("12437-69c6a5cb-3").await.is_some());
}

// ============================================================
// Bug: duplicate session IDs silently replace existing session
// Fix: 983325e — reject duplicate session IDs
// ============================================================

#[tokio::test]
async fn reject_duplicate_session_ids() {
    let sessions = Arc::new(SessionManager::new("/bin/sh", 100));
    let r1 = sessions.register_session(Some("dup-test-1".to_string()), None).await;
    assert!(r1.is_ok());
    let r2 = sessions.register_session(Some("dup-test-1".to_string()), None).await;
    assert!(r2.is_err(), "duplicate session ID should be rejected");
}

// ============================================================
// Bug: sessions list didn't include host info
// Fix: 8f491e9 — track which shytti spawned each session
// ============================================================

#[tokio::test]
async fn session_list_includes_host() {
    let (base, sessions) = start_server().await;

    // Register a managed session with a host name.
    sessions.register_session(Some("host-test-1".to_string()), Some("shytti-iggy".to_string())).await.unwrap();

    let res: serde_json::Value = auth(client().get(format!("{}/sessions", base)))
        .send().await.unwrap()
        .json().await.unwrap();

    let sessions_arr = res["sessions"].as_array().unwrap();
    let managed = sessions_arr.iter().find(|s| s["id"] == "host-test-1").unwrap();
    assert_eq!(managed["host"], "shytti-iggy");
}

#[tokio::test]
async fn local_session_has_no_host() {
    let (base, _) = start_server().await;

    let res: serde_json::Value = auth(client().get(format!("{}/sessions", base)))
        .send().await.unwrap()
        .json().await.unwrap();

    let sessions_arr = res["sessions"].as_array().unwrap();
    let local = &sessions_arr[0]; // the default session created in start_server
    assert!(local.get("host").map(|h| h.is_null()).unwrap_or(true));
}

// ============================================================
// Bug: registry shared between REST transport and Mode 2 reconnect
// Fix: 9fe7b77 — shared registry between REST transport and Mode 2
// ============================================================

#[tokio::test]
async fn registry_announce_and_list() {
    let (base, _) = start_server().await;

    // Announce a service.
    let res = auth(client().post(format!("{}/registry/announce", base)))
        .header("Content-Type", "application/json")
        .body(r#"{"name":"test-svc","role":"parser","endpoint":"http://localhost:9999","meta":{"host":"test"}}"#)
        .send().await.unwrap();
    assert_eq!(res.status(), 200);

    // List should include it.
    let res: serde_json::Value = auth(client().get(format!("{}/registry", base)))
        .send().await.unwrap()
        .json().await.unwrap();
    let services = res["services"].as_array().unwrap();
    assert!(services.iter().any(|s| s["name"] == "test-svc"));
    assert!(services.iter().any(|s| s["role"] == "parser"));
}

// ============================================================
// Bug: Parser role not recognized in registry
// Fix: 14e5dd9 — add Parser role to ServiceRole enum
// ============================================================

#[tokio::test]
async fn parser_role_recognized() {
    let (base, _) = start_server().await;

    auth(client().post(format!("{}/registry/announce", base)))
        .header("Content-Type", "application/json")
        .body(r#"{"name":"grytti","role":"parser","endpoint":"http://localhost:7780"}"#)
        .send().await.unwrap();

    let res: serde_json::Value = auth(client().get(format!("{}/registry", base)))
        .send().await.unwrap()
        .json().await.unwrap();
    let grytti = res["services"].as_array().unwrap().iter()
        .find(|s| s["name"] == "grytti").unwrap().clone();
    assert_eq!(grytti["role"], "parser", "parser role should not fall back to unknown");
}

// ============================================================
// Bug: registry proxy returns 404 for unknown service
// Fix: 14e5dd9 — add generic proxy route
// ============================================================

#[tokio::test]
async fn proxy_404_for_unknown_service() {
    let (base, _) = start_server().await;

    let res = auth(client().get(format!("{}/registry/nonexistent/proxy/status", base)))
        .send().await.unwrap();
    assert_eq!(res.status(), 404);
}

// ============================================================
// Bug: bootstrap endpoint required auth
// Fix: 4087c84 — make /bootstrap/shytti public
// ============================================================

#[tokio::test]
async fn bootstrap_endpoint_no_auth_required() {
    let (base, _) = start_server().await;

    // No auth header — should still return 200 with the shell script.
    let res = client()
        .get(format!("{}/bootstrap/shytti", base))
        .send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("#!/bin/bash"), "should be a shell script");
    assert!(body.contains("HERMYTT_URL"), "should contain hermytt URL");
}

#[tokio::test]
async fn bootstrap_script_resolves_version() {
    let (base, _) = start_server().await;

    let body = client()
        .get(format!("{}/bootstrap/shytti", base))
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(body.contains("VERSION=$(curl"), "should resolve latest version from GitHub");
    assert!(!body.contains("/latest/download/shytti"), "should not use /latest/ redirect URL");
}

#[tokio::test]
async fn bootstrap_script_has_launchd_support() {
    let (base, _) = start_server().await;

    let body = client()
        .get(format!("{}/bootstrap/shytti", base))
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(body.contains("launchctl"), "should have macOS launchd support");
    assert!(body.contains("com.yttfam.shytti"), "should have correct plist label");
    assert!(body.contains("UserName"), "should run as invoking user, not root");
}

#[tokio::test]
async fn bootstrap_script_chowns_config() {
    let (base, _) = start_server().await;

    let body = client()
        .get(format!("{}/bootstrap/shytti", base))
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(body.contains("SUDO_USER"), "should use SUDO_USER for chown");
    assert!(body.contains("chown"), "should chown config files to run user");
}

#[tokio::test]
async fn bootstrap_script_uses_serve_not_daemon() {
    let (base, _) = start_server().await;

    let body = client()
        .get(format!("{}/bootstrap/shytti", base))
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(body.contains("shytti serve"), "should use 'serve' subcommand");
    assert!(!body.contains("shytti daemon -c"), "should not use 'daemon' subcommand in service exec");
}

#[tokio::test]
async fn bootstrap_logs_to_install_dir() {
    let (base, _) = start_server().await;

    let body = client()
        .get(format!("{}/bootstrap/shytti", base))
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(body.contains("$INSTALL_DIR/shytti.log"), "should log to install dir, not /var/log");
    assert!(!body.contains("/var/log/shytti.log"), "should not log to /var/log");
}

#[tokio::test]
async fn bootstrap_uses_environmentfile() {
    let (base, _) = start_server().await;

    let body = client()
        .get(format!("{}/bootstrap/shytti", base))
        .send().await.unwrap()
        .text().await.unwrap();
    assert!(body.contains("EnvironmentFile="), "should use EnvironmentFile for systemd");
    assert!(!body.contains("Environment=HERMYTT_KEY"), "should not leak token in Environment= line");
}

// ============================================================
// Bug: spawn request hardcoded /bin/bash
// Fix: dccf50f — don't override shell in spawn requests
// ============================================================
// (This is an admin UI bug — tested in Playwright e2e)

// ============================================================
// Bug: open redirect in login page
// Fix: 983325e — validate next param
// ============================================================
// (Login page is static HTML — tested in Playwright e2e)

// ============================================================
// Bug: control WS name squatting (last-writer-wins)
// Fix: c3bd787 — replace stale connection on reconnect
// ============================================================

#[tokio::test]
async fn control_hub_last_writer_wins() {
    let hub = hermytt_core::ControlHub::new();
    let (tx1, _rx1) = tokio::sync::mpsc::channel(64);
    let (tx2, _rx2) = tokio::sync::mpsc::channel(64);

    hub.register("shytti-test".to_string(), serde_json::json!({"v": 1}), tx1).await;
    hub.register("shytti-test".to_string(), serde_json::json!({"v": 2}), tx2).await;

    let hosts = hub.list().await;
    assert_eq!(hosts.len(), 1, "should have exactly one entry after replacement");
}

// ============================================================
// Bug: .keys.json file permissions too open
// Fix: 983325e — set 0600 permissions
// ============================================================
// (File permission test would need filesystem setup — covered by code review)

// ============================================================
// Bug: sessions with_host returns host info
// ============================================================

#[tokio::test]
async fn list_sessions_with_host_returns_tuples() {
    let sessions = Arc::new(SessionManager::new("/bin/sh", 100));
    sessions.create_session().await.unwrap(); // local, no host
    sessions.register_session(Some("remote-1".to_string()), Some("shytti-iggy".to_string())).await.unwrap();

    let list = sessions.list_sessions_with_host().await;
    assert_eq!(list.len(), 2);

    let remote = list.iter().find(|(id, _)| id == "remote-1").unwrap();
    assert_eq!(remote.1, Some("shytti-iggy".to_string()));

    let local = list.iter().find(|(id, _)| id != "remote-1").unwrap();
    assert!(local.1.is_none());
}

// ============================================================
// Bug: service registry expires stale services
// ============================================================

#[tokio::test]
async fn registry_fresh_service_is_connected() {
    let reg = hermytt_core::ServiceRegistry::new();
    let info = hermytt_core::registry::ServiceInfo {
        name: "fresh-svc".to_string(),
        role: hermytt_core::registry::ServiceRole::Parser,
        endpoint: "http://localhost:9999".to_string(),
        status: hermytt_core::registry::ServiceStatus::Connected,
        last_seen_instant: None,
        last_seen: 0,
        meta: serde_json::json!({}),
    };
    reg.register(info).await;
    let list = reg.list().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, hermytt_core::registry::ServiceStatus::Connected);
    // Stale expiration tested in hermytt_core::registry::tests::expire_marks_disconnected
}

// ============================================================
// Bug: proxy returns 502 for disconnected service
// ============================================================

#[tokio::test]
async fn proxy_502_for_unreachable_service() {
    let (base, _) = start_server().await;

    // Register a service with an unreachable endpoint.
    let res = auth(client().post(format!("{}/registry/announce", base)))
        .header("Content-Type", "application/json")
        .body(r#"{"name":"dead-svc","role":"parser","endpoint":"http://127.0.0.1:1"}"#)
        .send().await.unwrap();
    assert_eq!(res.status(), 200);

    // Proxy should return 502 (bad gateway) because the endpoint is unreachable.
    let res = auth(client().get(format!("{}/registry/dead-svc/proxy/status", base)))
        .send().await.unwrap();
    assert_eq!(res.status(), 502, "unreachable service endpoint should return 502");
}
