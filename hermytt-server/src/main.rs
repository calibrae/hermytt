mod config;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hermytt_core::SessionManager;
use hermytt_transport::Transport;
use hermytt_transport::mqtt::MqttTransport;
use hermytt_transport::rest::RestTransport;
use hermytt_transport::tcp::TcpTransport;
use hermytt_transport::tls::TlsConfig;
use tracing::{error, info, warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "hermytt",
    version = VERSION,
    about = "The hermit TTY — transport-agnostic terminal multiplexer",
    long_about = "One PTY session. Multiple transports. Any client that speaks text is a terminal.\n\n\
                   REST, WebSocket, MQTT, raw TCP — the hermit lives alone but talks to everyone.",
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the hermytt server (default if no subcommand given)
    Start {
        /// Path to config file (TOML)
        #[arg(short, long)]
        config: Option<String>,

        /// Override shell (e.g., /bin/zsh)
        #[arg(short, long)]
        shell: Option<String>,

        /// Override bind address (e.g., 0.0.0.0)
        #[arg(short, long)]
        bind: Option<String>,
    },

    /// Generate a random auth token
    GenToken,

    /// Print an example config file
    ExampleConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command.unwrap_or(Commands::Start {
        config: None,
        shell: None,
        bind: None,
    }) {
        Commands::GenToken => {
            let token = uuid::Uuid::new_v4().to_string().replace('-', "");
            println!("{token}");
            return Ok(());
        }
        Commands::ExampleConfig => {
            print!("{}", include_str!("../../hermytt.example.toml"));
            return Ok(());
        }
        Commands::Start {
            config: config_path,
            shell,
            bind,
        } => {
            start_server(config_path.as_deref(), shell, bind).await?;
        }
    }

    Ok(())
}

async fn start_server(
    config_path: Option<&str>,
    shell_override: Option<String>,
    bind_override: Option<String>,
) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut config = config::Config::load(config_path)?;

    if let Some(shell) = shell_override {
        config.server.shell = shell;
    }
    if let Some(bind) = bind_override {
        config.server.bind = bind;
    }

    let auth_token = config.auth.token.clone();

    if auth_token.is_none() {
        warn!("no auth token configured — all transports are unauthenticated!");
        warn!("generate one with: hermytt-server gen-token");
    }

    // Load TLS config if both cert and key are specified.
    let tls_config = match (&config.server.tls_cert, &config.server.tls_key) {
        (Some(cert), Some(key)) => {
            let tls = TlsConfig::from_pem(cert, key)?;
            info!(cert = %cert, key = %key, "TLS enabled");
            Some(tls)
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("both tls_cert and tls_key must be set to enable TLS");
        }
        _ => None,
    };

    info!(version = VERSION, "hermytt starting up");

    let sessions = Arc::new(SessionManager::new(
        &config.server.shell,
        config.server.scrollback,
    ));

    let recording_dir = config
        .server
        .recording_dir
        .as_ref()
        .map(std::path::PathBuf::from);

    let mut tasks = Vec::new();

    // Wait for a shell orchestrator (Shytti) to register. If none arrives
    // within 5 seconds, spawn a local PTY session as fallback.
    let fallback_sessions = sessions.clone();
    let fallback_recording_dir = recording_dir.clone();
    let auto_record = config.server.auto_record;
    tasks.push(tokio::spawn(async move {
        info!("waiting 5s for shell orchestrator...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // Check if any sessions exist (Shytti would have registered some).
        let existing = fallback_sessions.list_sessions().await;
        if !existing.is_empty() {
            info!(count = existing.len(), "shell orchestrator active — skipping local PTY");
            return;
        }

        info!("no shell orchestrator found — spawning local PTY");
        match fallback_sessions.create_session().await {
            Ok(handle) => {
                info!(session = %handle.id, "fallback session ready");
                if auto_record {
                    if let Some(ref dir) = fallback_recording_dir {
                        let recordings = std::sync::Arc::new(tokio::sync::Mutex::new(
                            std::collections::HashMap::new(),
                        ));
                        hermytt_transport::rest::auto_record_session(&handle, dir, &recordings).await;
                    }
                }
            }
            Err(e) => error!(error = %e, "failed to create fallback session"),
        }
    }));

    // Periodic dead session cleanup.
    let cleanup_sessions = sessions.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            cleanup_sessions.cleanup_dead().await;
        }
    }));

    // Build transport info for the admin dashboard.
    let mut transport_info = Vec::new();
    if let Some(r) = &config.transport.rest {
        transport_info.push(hermytt_transport::rest::TransportInfo {
            name: "REST + WebSocket".into(),
            endpoint: format!("{}:{}", config.server.bind, r.port),
        });
    }
    if let Some(m) = &config.transport.mqtt {
        transport_info.push(hermytt_transport::rest::TransportInfo {
            name: "MQTT".into(),
            endpoint: format!("{}:{}", m.broker, m.port),
        });
    }
    if let Some(t) = &config.transport.tcp {
        transport_info.push(hermytt_transport::rest::TransportInfo {
            name: "TCP".into(),
            endpoint: format!("{}:{}", config.server.bind, t.port),
        });
    }
    if let Some(rest_config) = &config.transport.rest {
        let transport = Arc::new(RestTransport {
            port: rest_config.port,
            bind: config.server.bind.clone(),
            auth_token: auth_token.clone(),
            shell: config.server.shell.clone(),
            transport_info: transport_info.clone(),
            config_path: config_path.map(String::from),
            tls: tls_config.clone(),
            recording_dir: config.server.recording_dir.as_ref().map(std::path::PathBuf::from),
            files_dir: config.server.files_dir.as_ref().map(std::path::PathBuf::from),
            max_upload_size: config.server.max_upload_size,
            extra_routes: Some(hermytt_web::routes()),
        });
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = transport.serve(sessions).await {
                tracing::error!(transport = "rest", error = %e, "transport failed");
            }
        }));
    }


    if let Some(mqtt_config) = &config.transport.mqtt {
        let transport = Arc::new(MqttTransport {
            broker_host: mqtt_config.broker.clone(),
            broker_port: mqtt_config.port,
            username: mqtt_config.username.clone(),
            password: mqtt_config.password.clone(),
        });
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = transport.serve(sessions).await {
                tracing::error!(transport = "mqtt", error = %e, "transport failed");
            }
        }));
    }

    if let Some(tcp_config) = &config.transport.tcp {
        let transport = Arc::new(TcpTransport {
            port: tcp_config.port,
            bind: config.server.bind.clone(),
            auth_token: auth_token.clone(),
            tls: tls_config.clone(),
        });
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = transport.serve(sessions).await {
                tracing::error!(transport = "tcp", error = %e, "transport failed");
            }
        }));
    }

    if tasks.is_empty() {
        warn!("no transports enabled — hermytt has nothing to do");
        warn!("add at least [transport.rest] to your config");
        return Ok(());
    }

    futures_util::future::join_all(tasks).await;

    Ok(())
}
