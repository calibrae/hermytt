use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tracing::{error, info, warn};

pub type SessionId = String;

/// A handle that transports use to interact with a session.
#[derive(Clone)]
pub struct SessionHandle {
    pub id: SessionId,
    pub stdin_tx: mpsc::Sender<Vec<u8>>,
    pub output_tx: broadcast::Sender<Vec<u8>>,
    pub scrollback: Arc<Mutex<ScrollbackBuffer>>,
}

impl SessionHandle {
    pub fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    pub fn subscribe_buffered(
        &self,
        window: std::time::Duration,
    ) -> crate::buffer::BufferedOutput {
        crate::buffer::BufferedOutput::new(self.output_tx.subscribe(), window)
    }

    /// Execute a command and collect output until the shell goes quiet.
    ///
    /// Returns raw bytes. Caller is responsible for decoding/stripping ANSI.
    /// `silence` is how long to wait with no output before considering the
    /// command done (e.g., 500ms).
    pub async fn execute(&self, cmd: &str, silence: std::time::Duration) -> Result<Vec<u8>> {
        let mut rx = self.output_tx.subscribe();

        // Send the command.
        let mut input = cmd.to_string();
        if !input.ends_with('\n') {
            input.push('\n');
        }
        self.stdin_tx
            .send(input.into_bytes())
            .await
            .map_err(|_| anyhow::anyhow!("session stdin closed"))?;

        // Collect output until silence.
        let mut output = Vec::new();
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(data) => output.extend_from_slice(&data),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = tokio::time::sleep(silence) => {
                    // No output for `silence` duration — command is done.
                    break;
                }
            }
        }

        Ok(output)
    }
}

const MAX_LINE_LEN: usize = 4096;

pub struct ScrollbackBuffer {
    lines: VecDeque<String>,
    capacity: usize,
}

impl ScrollbackBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, data: &str) {
        for line in data.split('\n') {
            if self.lines.len() >= self.capacity {
                self.lines.pop_front();
            }
            let truncated = if line.len() > MAX_LINE_LEN {
                &line[..MAX_LINE_LEN]
            } else {
                line
            };
            self.lines.push_back(truncated.to_string());
        }
    }

    pub fn get_all(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

/// A running PTY session.
pub struct Session {
    pub handle: SessionHandle,
    stdin_rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    child: Mutex<Option<Box<dyn Child + Send>>>,
    shell: String,
}

impl Session {
    pub fn new(shell: &str, scrollback_capacity: usize) -> Result<Arc<Self>> {
        let id = uuid::Uuid::new_v4().to_string().replace('-', "")[..16].to_string();
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(256);
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(256);
        let scrollback = Arc::new(Mutex::new(ScrollbackBuffer::new(scrollback_capacity)));

        let handle = SessionHandle {
            id: id.clone(),
            stdin_tx,
            output_tx,
            scrollback,
        };

        Ok(Arc::new(Self {
            handle,
            stdin_rx: Mutex::new(Some(stdin_rx)),
            child: Mutex::new(None),
            shell: shell.to_string(),
        }))
    }

    /// Spawn the PTY and start I/O loops. Call once.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        let mut stdin_rx = self
            .stdin_rx
            .lock()
            .await
            .take()
            .context("session already started")?;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let mut cmd = CommandBuilder::new(&self.shell);
        crate::platform::configure_command(&mut cmd);
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell")?;

        *self.child.lock().await = Some(child);
        drop(pair.slave);

        let mut master_writer = pair.master.take_writer()?;
        let mut master_reader = pair.master.try_clone_reader()?;

        let output_tx = self.handle.output_tx.clone();
        let scrollback = self.handle.scrollback.clone();
        let session_id = self.handle.id.clone();

        // PTY stdout -> broadcast
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match master_reader.read(&mut buf) {
                    Ok(0) => {
                        info!(session = %session_id, "PTY closed");
                        break;
                    }
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if let Ok(text) = std::str::from_utf8(&data) {
                            if let Ok(mut sb) = scrollback.try_lock() {
                                sb.push(text);
                            }
                        }
                        let _ = output_tx.send(data);
                    }
                    Err(e) => {
                        error!(session = %session_id, error = %e, "PTY read error");
                        break;
                    }
                }
            }
        });

        // stdin channel -> PTY stdin
        tokio::spawn(async move {
            while let Some(data) = stdin_rx.recv().await {
                if master_writer.write_all(&data).is_err() {
                    break;
                }
            }
        });

        info!(session = %self.handle.id, shell = %self.shell, "session started");
        Ok(())
    }

    pub async fn is_alive(&self) -> bool {
        if let Some(child) = self.child.lock().await.as_mut() {
            child.try_wait().ok().flatten().is_none()
        } else {
            false
        }
    }
}

/// Manages multiple sessions.
pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
    shell: String,
    scrollback_capacity: usize,
    max_sessions: usize,
}

impl SessionManager {
    pub fn new(shell: &str, scrollback_capacity: usize) -> Self {
        Self::with_max_sessions(shell, scrollback_capacity, 16)
    }

    pub fn with_max_sessions(shell: &str, scrollback_capacity: usize, max_sessions: usize) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            shell: shell.to_string(),
            scrollback_capacity,
            max_sessions,
        }
    }

    pub async fn create_session(&self) -> Result<SessionHandle> {
        let sessions = self.sessions.read().await;
        anyhow::ensure!(
            sessions.len() < self.max_sessions,
            "max sessions ({}) reached",
            self.max_sessions
        );
        drop(sessions);

        let session = Session::new(&self.shell, self.scrollback_capacity)?;
        session.start().await?;
        let handle = session.handle.clone();
        self.sessions
            .write()
            .await
            .insert(handle.id.clone(), session);
        info!(session = %handle.id, "session created");
        Ok(handle)
    }

    pub async fn get_session(&self, id: &str) -> Option<SessionHandle> {
        self.sessions.read().await.get(id).map(|s| s.handle.clone())
    }

    pub async fn list_sessions(&self) -> Vec<SessionId> {
        self.sessions.read().await.keys().cloned().collect()
    }

    pub async fn default_session(&self) -> Result<SessionHandle> {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.values().next() {
            return Ok(session.handle.clone());
        }
        drop(sessions);
        self.create_session().await
    }

    pub async fn cleanup_dead(&self) -> usize {
        let sessions = self.sessions.read().await;
        let mut dead = Vec::new();
        for (id, session) in sessions.iter() {
            if !session.is_alive().await {
                dead.push(id.clone());
            }
        }
        drop(sessions);

        let count = dead.len();
        if count > 0 {
            let mut sessions = self.sessions.write().await;
            for id in &dead {
                sessions.remove(id);
                warn!(session = %id, "removed dead session");
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_basic() {
        let mut sb = ScrollbackBuffer::new(5);
        sb.push("line1\nline2\nline3");
        assert_eq!(sb.get_all(), vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn scrollback_evicts_oldest() {
        let mut sb = ScrollbackBuffer::new(3);
        sb.push("a\nb\nc");
        sb.push("d");
        // "a\nb\nc" produces ["a","b","c"], then "d" pushes out "a"
        assert_eq!(sb.get_all(), vec!["b", "c", "d"]);
    }

    #[test]
    fn scrollback_truncates_long_lines() {
        let mut sb = ScrollbackBuffer::new(10);
        let long = "x".repeat(MAX_LINE_LEN + 100);
        sb.push(&long);
        assert_eq!(sb.get_all()[0].len(), MAX_LINE_LEN);
    }

    #[test]
    fn scrollback_capacity_one() {
        let mut sb = ScrollbackBuffer::new(1);
        sb.push("first");
        sb.push("second");
        assert_eq!(sb.get_all(), vec!["second"]);
        assert_eq!(sb.len(), 1);
    }

    #[test]
    fn scrollback_empty_lines() {
        let mut sb = ScrollbackBuffer::new(10);
        sb.push("\n\n");
        assert_eq!(sb.get_all(), vec!["", "", ""]);
    }
}
