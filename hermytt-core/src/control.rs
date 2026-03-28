use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{error, info, warn};

/// A connected shytti instance.
#[derive(Debug)]
pub struct ShyttiConnection {
    pub name: String,
    pub meta: serde_json::Value,
    /// Send control messages to this shytti.
    pub tx: mpsc::Sender<ControlMessage>,
}

/// Messages hermytt sends to shytti.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Spawn {
        req_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        shell: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Kill {
        shell_id: String,
    },
    Resize {
        shell_id: String,
        cols: u16,
        rows: u16,
    },
    /// Stdin data for a session (Mode 2 data plane).
    Input {
        session_id: String,
        data: String, // base64
    },
    /// Ask shytti to list all active shells.
    ListShells {},
}

/// Messages shytti sends to hermytt.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShyttiMessage {
    Heartbeat {
        #[serde(default)]
        meta: serde_json::Value,
    },
    SpawnOk {
        req_id: String,
        shell_id: String,
        session_id: String,
    },
    SpawnErr {
        req_id: String,
        error: String,
    },
    KillOk {
        shell_id: String,
    },
    ShellDied {
        shell_id: String,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// PTY output from shytti (Mode 2 data plane).
    Data {
        session_id: String,
        data: String, // base64
    },
    /// Response to ListShells — all active shells on this shytti.
    ShellsList {
        shells: Vec<ShellInfo>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShellInfo {
    pub shell_id: String,
    pub session_id: String,
}

/// Pending spawn request waiting for a response.
struct PendingSpawn {
    tx: oneshot::Sender<Result<SpawnResult, String>>,
}

#[derive(Debug, Clone)]
pub struct SpawnResult {
    pub shell_id: String,
    pub session_id: String,
}

/// Manages all connected shytti instances.
pub struct ControlHub {
    connections: Mutex<HashMap<String, ShyttiConnection>>,
    pending_spawns: Mutex<HashMap<String, PendingSpawn>>,
}

impl ControlHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: Mutex::new(HashMap::new()),
            pending_spawns: Mutex::new(HashMap::new()),
        })
    }

    /// Register a shytti connection. Last-writer-wins: if name exists, the old connection is replaced.
    pub async fn register(&self, name: String, meta: serde_json::Value, tx: mpsc::Sender<ControlMessage>) {
        let mut conns = self.connections.lock().await;
        if conns.contains_key(&name) {
            warn!(name = %name, "replacing stale connection (reconnect)");
        }
        info!(name = %name, "shytti connected");
        conns.insert(name.clone(), ShyttiConnection { name, meta, tx });
    }

    /// Remove a shytti connection.
    pub async fn unregister(&self, name: &str) {
        self.connections.lock().await.remove(name);
        info!(name = %name, "shytti disconnected");
    }

    /// Update metadata (from heartbeat).
    pub async fn heartbeat(&self, name: &str, meta: serde_json::Value) {
        if let Some(conn) = self.connections.lock().await.get_mut(name) {
            conn.meta = meta;
        }
    }

    /// List connected shytti instances.
    pub async fn list(&self) -> Vec<(String, serde_json::Value)> {
        self.connections.lock().await.iter()
            .map(|(name, conn)| (name.clone(), conn.meta.clone()))
            .collect()
    }

    /// Request a shell spawn on a specific shytti instance.
    /// Returns the spawn result or an error.
    pub async fn spawn(
        &self,
        host: &str,
        shell: Option<String>,
        cwd: Option<String>,
    ) -> Result<SpawnResult, String> {
        let req_id = uuid::Uuid::new_v4().to_string()[..8].to_string();

        let tx = {
            let conns = self.connections.lock().await;
            let conn = conns.get(host).ok_or_else(|| format!("host '{}' not connected", host))?;
            conn.tx.clone()
        };

        // Set up response channel.
        let (resp_tx, resp_rx) = oneshot::channel();
        self.pending_spawns.lock().await.insert(req_id.clone(), PendingSpawn { tx: resp_tx });

        // Send spawn request.
        let msg = ControlMessage::Spawn {
            req_id: req_id.clone(),
            shell,
            cwd,
            session_id: None,
        };

        if tx.send(msg).await.is_err() {
            self.pending_spawns.lock().await.remove(&req_id);
            return Err("shytti connection lost".to_string());
        }

        // Wait for response (timeout 10s).
        match tokio::time::timeout(std::time::Duration::from_secs(10), resp_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.pending_spawns.lock().await.remove(&req_id);
                Err("response channel dropped".to_string())
            }
            Err(_) => {
                self.pending_spawns.lock().await.remove(&req_id);
                Err("spawn timed out (10s)".to_string())
            }
        }
    }

    /// Handle a spawn response from shytti.
    pub async fn handle_spawn_ok(&self, req_id: &str, shell_id: String, session_id: String) {
        if let Some(pending) = self.pending_spawns.lock().await.remove(req_id) {
            let _ = pending.tx.send(Ok(SpawnResult { shell_id, session_id }));
        }
    }

    /// Handle a spawn error from shytti.
    pub async fn handle_spawn_err(&self, req_id: &str, error: String) {
        if let Some(pending) = self.pending_spawns.lock().await.remove(req_id) {
            let _ = pending.tx.send(Err(error));
        }
    }

    /// Send any control message to a specific shytti.
    pub async fn send_to(&self, host: &str, msg: ControlMessage) -> Result<(), String> {
        let conns = self.connections.lock().await;
        let conn = conns.get(host).ok_or_else(|| format!("host '{}' not connected", host))?;
        conn.tx.send(msg).await.map_err(|_| "connection lost".to_string())
    }

    /// Send a kill command to the shytti that owns a shell.
    pub async fn kill(&self, host: &str, shell_id: &str) -> Result<(), String> {
        let conns = self.connections.lock().await;
        let conn = conns.get(host).ok_or_else(|| format!("host '{}' not connected", host))?;
        conn.tx.send(ControlMessage::Kill { shell_id: shell_id.to_string() })
            .await
            .map_err(|_| "connection lost".to_string())
    }

    /// Send a resize command.
    pub async fn resize(&self, host: &str, shell_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let conns = self.connections.lock().await;
        let conn = conns.get(host).ok_or_else(|| format!("host '{}' not connected", host))?;
        conn.tx.send(ControlMessage::Resize { shell_id: shell_id.to_string(), cols, rows })
            .await
            .map_err(|_| "connection lost".to_string())
    }

    /// Check if any shytti instances are connected.
    pub async fn has_connections(&self) -> bool {
        !self.connections.lock().await.is_empty()
    }
}
