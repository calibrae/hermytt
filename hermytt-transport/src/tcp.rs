use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use hermytt_core::SessionManager;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::Transport;
use crate::tls::TlsConfig;

pub struct TcpTransport {
    pub port: u16,
    pub bind: String,
    pub auth_token: Option<String>,
    pub tls: Option<TlsConfig>,
}

#[async_trait]
impl Transport for TcpTransport {
    async fn serve(self: Arc<Self>, sessions: Arc<SessionManager>) -> Result<()> {
        let addr = format!("{}:{}", self.bind, self.port);
        let listener = TcpListener::bind(&addr).await?;
        let tls_label = if self.tls.is_some() { " (TLS)" } else { "" };
        info!(transport = "tcp", addr = %addr, "listening{}", tls_label);

        loop {
            let (stream, peer) = listener.accept().await?;
            info!(transport = "tcp", peer = %peer, "client connected");

            let sessions = sessions.clone();
            let auth_token = self.auth_token.clone();
            let tls = self.tls.clone();

            tokio::spawn(async move {
                let result = if let Some(tls) = tls {
                    match tls.acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            let (reader, writer) = tokio::io::split(tls_stream);
                            handle_connection(reader, writer, sessions, auth_token).await
                        }
                        Err(e) => {
                            error!(transport = "tcp", peer = %peer, error = %e, "TLS handshake failed");
                            return;
                        }
                    }
                } else {
                    let (reader, writer) = stream.into_split();
                    handle_connection(reader, writer, sessions, auth_token).await
                };

                if let Err(e) = result {
                    error!(transport = "tcp", peer = %peer, error = %e, "connection error");
                }
                info!(transport = "tcp", peer = %peer, "client disconnected");
            });
        }
    }

    fn name(&self) -> &str {
        "tcp"
    }
}

async fn handle_connection<R, W>(
    reader: R,
    mut writer: W,
    sessions: Arc<SessionManager>,
    auth_token: Option<String>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);

    // First-line token auth.
    if let Some(expected) = &auth_token {
        writer.write_all(b"token: ").await?;
        let mut line = String::new();
        // Cap read to 512 bytes to prevent memory exhaustion before auth.
        (&mut reader).take(512).read_line(&mut line).await?;
        let provided = line.trim();
        if provided != expected {
            warn!(transport = "tcp", "unauthorized connection");
            writer.write_all(b"unauthorized\n").await?;
            return Ok(());
        }
        writer.write_all(b"ok\n").await?;
    }

    let handle = sessions.default_session().await?;
    let stdin_tx = handle.stdin_tx.clone();
    let mut output = handle.subscribe_buffered(Duration::ZERO);

    let mut write_task = tokio::spawn(async move {
        while let Some(data) = output.recv().await {
            if writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    let mut read_task = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // M6 fix: abort the other task when one finishes.
    tokio::select! {
        _ = &mut write_task => { read_task.abort(); }
        _ = &mut read_task => { write_task.abort(); }
    }

    Ok(())
}
