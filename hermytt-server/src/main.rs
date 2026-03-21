mod config;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hermytt_core::SessionManager;
use hermytt_transport::Transport;
use hermytt_transport::mqtt::MqttTransport;
use hermytt_transport::rest::RestTransport;
use hermytt_transport::tcp::TcpTransport;
use hermytt_transport::telegram::TelegramTransport;
use hermytt_transport::websocket::WebSocketTransport;
use tracing::{info, warn};

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

    info!(version = VERSION, "hermytt starting up");

    let sessions = Arc::new(SessionManager::new(
        &config.server.shell,
        config.server.scrollback,
    ));

    let default = sessions.create_session().await?;
    info!(session = %default.id, "default session ready");

    let mut tasks = Vec::new();

    if let Some(rest_config) = &config.transport.rest {
        let ws_port = config
            .transport
            .websocket
            .as_ref()
            .map(|c| c.port)
            .unwrap_or(7778);
        let transport = Arc::new(RestTransport {
            port: rest_config.port,
            bind: config.server.bind.clone(),
            ws_port,
            auth_token: auth_token.clone(),
        });
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = transport.serve(sessions).await {
                tracing::error!(transport = "rest", error = %e, "transport failed");
            }
        }));
    }

    if let Some(ws_config) = &config.transport.websocket {
        let transport = Arc::new(WebSocketTransport {
            port: ws_config.port,
            bind: config.server.bind.clone(),
            auth_token: auth_token.clone(),
        });
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = transport.serve(sessions).await {
                tracing::error!(transport = "websocket", error = %e, "transport failed");
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
        });
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = transport.serve(sessions).await {
                tracing::error!(transport = "tcp", error = %e, "transport failed");
            }
        }));
    }

    if let Some(tg_config) = &config.transport.telegram {
        let transport = Arc::new(TelegramTransport {
            bot_token: tg_config.bot_token.clone(),
            chat_ids: tg_config.chat_ids.clone(),
        });
        let sessions = sessions.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = transport.serve(sessions).await {
                tracing::error!(transport = "telegram", error = %e, "transport failed");
            }
        }));
    }

    futures_util::future::join_all(tasks).await;

    Ok(())
}
