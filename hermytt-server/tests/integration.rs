use std::sync::Arc;
use std::time::Duration;

use hermytt_core::SessionManager;
use hermytt_transport::Transport;
use hermytt_transport::rest::{RestTransport, TransportInfo};

const TOKEN: &str = "test-token-123";

/// Start a REST server on a random port, return the base URL.
async fn start_server() -> String {
    let sessions = Arc::new(SessionManager::new("/bin/sh", 100));
    sessions.create_session().await.unwrap();

    // Find a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let transport = Arc::new(RestTransport {
        port,
        bind: "127.0.0.1".to_string(),
        auth_token: Some(TOKEN.to_string()),
        shell: "/bin/sh".to_string(),
        transport_info: vec![TransportInfo {
            name: "REST".into(),
            endpoint: format!("127.0.0.1:{}", port),
        }],
    });

    let sessions_clone = sessions.clone();
    tokio::spawn(async move {
        transport.serve(sessions_clone).await.unwrap();
    });

    // Wait for server to be ready.
    let base = format!("http://127.0.0.1:{}", port);
    for _ in 0..50 {
        if reqwest::get(&base).await.is_ok() {
            return base;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server didn't start");
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

// --- Web UI ---

#[tokio::test]
async fn web_terminal_page_loads() {
    let base = start_server().await;
    let res = client().get(&base).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("hermytt"));
    assert!(body.contains("xterm"));
}

#[tokio::test]
async fn web_admin_page_loads() {
    let base = start_server().await;
    let res = client().get(format!("{}/admin", base)).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("admin"));
    assert!(body.contains("Sessions"));
}

// --- Auth ---

#[tokio::test]
async fn auth_rejects_no_token() {
    let base = start_server().await;
    let res = client()
        .get(format!("{}/sessions", base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn auth_rejects_wrong_token() {
    let base = start_server().await;
    let res = client()
        .get(format!("{}/sessions", base))
        .header("X-Hermytt-Key", "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn auth_accepts_header_token() {
    let base = start_server().await;
    let res = client()
        .get(format!("{}/sessions", base))
        .header("X-Hermytt-Key", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn auth_accepts_query_token() {
    let base = start_server().await;
    let res = client()
        .get(format!("{}/sessions?token={}", base, TOKEN))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

// --- Sessions ---

#[tokio::test]
async fn list_sessions_returns_default() {
    let base = start_server().await;
    let res: serde_json::Value = client()
        .get(format!("{}/sessions?token={}", base, TOKEN))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sessions = res["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
}

#[tokio::test]
async fn create_session() {
    let base = start_server().await;

    // Create a new session.
    let res: serde_json::Value = client()
        .post(format!("{}/session?token={}", base, TOKEN))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = res["id"].as_str().unwrap();
    assert_eq!(id.len(), 16);

    // List should now have 2.
    let res: serde_json::Value = client()
        .get(format!("{}/sessions?token={}", base, TOKEN))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["sessions"].as_array().unwrap().len(), 2);
}

// --- Exec ---

#[tokio::test]
async fn exec_whoami() {
    let base = start_server().await;
    let res: serde_json::Value = client()
        .post(format!("{}/exec?token={}", base, TOKEN))
        .header("Content-Type", "application/json")
        .body(r#"{"input": "whoami"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["exit_code"], 0);
    assert!(!res["stdout"].as_str().unwrap().is_empty());
    // No ANSI junk.
    assert!(!res["stdout"].as_str().unwrap().contains('\x1b'));
}

#[tokio::test]
async fn exec_echo() {
    let base = start_server().await;
    let res: serde_json::Value = client()
        .post(format!("{}/exec?token={}", base, TOKEN))
        .header("Content-Type", "application/json")
        .body(r#"{"input": "echo hello-hermytt"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["exit_code"], 0);
    assert_eq!(res["stdout"].as_str().unwrap().trim(), "hello-hermytt");
    assert!(res["stderr"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn exec_captures_stderr() {
    let base = start_server().await;
    let res: serde_json::Value = client()
        .post(format!("{}/exec?token={}", base, TOKEN))
        .header("Content-Type", "application/json")
        .body(r#"{"input": "echo err >&2"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["stderr"].as_str().unwrap().trim(), "err");
}

#[tokio::test]
async fn exec_nonzero_exit() {
    let base = start_server().await;
    let res: serde_json::Value = client()
        .post(format!("{}/exec?token={}", base, TOKEN))
        .header("Content-Type", "application/json")
        .body(r#"{"input": "exit 42"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["exit_code"], 42);
}

#[tokio::test]
async fn exec_multiline_output() {
    let base = start_server().await;
    let res: serde_json::Value = client()
        .post(format!("{}/exec?token={}", base, TOKEN))
        .header("Content-Type", "application/json")
        .body(r#"{"input": "seq 1 5"}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["exit_code"], 0);
    let stdout = res["stdout"].as_str().unwrap();
    assert!(stdout.contains("1"));
    assert!(stdout.contains("5"));
}

// --- Info ---

#[tokio::test]
async fn info_endpoint() {
    let base = start_server().await;
    let res: serde_json::Value = client()
        .get(format!("{}/info?token={}", base, TOKEN))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res["shell"], "/bin/sh");
    assert_eq!(res["sessions"], 1);
    assert!(res["transports"].as_array().unwrap().len() > 0);
}

// --- Stdin/Stdout (PTY stream) ---

#[tokio::test]
async fn stdin_to_pty_session() {
    let base = start_server().await;

    // Get session ID.
    let res: serde_json::Value = client()
        .get(format!("{}/sessions?token={}", base, TOKEN))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = res["sessions"][0]["id"].as_str().unwrap();

    // Write to stdin.
    let status = client()
        .post(format!("{}/session/{}/stdin?token={}", base, id, TOKEN))
        .header("Content-Type", "application/json")
        .body(r#"{"input": "echo test-pty"}"#)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200);
}
