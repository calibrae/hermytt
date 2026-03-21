use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub transport: TransportConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct AuthConfig {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default = "default_scrollback")]
    pub scrollback: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            shell: default_shell(),
            scrollback: default_scrollback(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct TransportConfig {
    #[serde(default)]
    pub rest: Option<RestConfig>,
    #[serde(default)]
    pub websocket: Option<WebSocketConfig>,
    #[serde(default)]
    pub mqtt: Option<MqttConfig>,
    #[serde(default)]
    pub tcp: Option<TcpConfig>,
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
}

#[derive(Debug, Deserialize)]
pub struct RestConfig {
    #[serde(default = "default_rest_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct WebSocketConfig {
    #[serde(default = "default_ws_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct MqttConfig {
    pub broker: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TcpConfig {
    #[serde(default = "default_tcp_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    #[serde(default)]
    pub chat_ids: Vec<i64>,
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}
fn default_shell() -> String {
    hermytt_core::platform::default_shell().to_string()
}
fn default_scrollback() -> usize {
    1000
}
fn default_rest_port() -> u16 {
    7777
}
fn default_ws_port() -> u16 {
    7778
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_tcp_port() -> u16 {
    7779
}

impl Config {
    pub fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(s)?)
    }

    pub fn load(path: Option<&str>) -> anyhow::Result<Self> {
        match path {
            Some(p) => {
                let content = std::fs::read_to_string(p)?;
                Self::from_str(&content)
            }
            None => Ok(Self::default_config()),
        }
    }

    fn default_config() -> Self {
        Config {
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            transport: TransportConfig {
                rest: Some(RestConfig { port: 7777 }),
                websocket: Some(WebSocketConfig { port: 7778 }),
                mqtt: None,
                tcp: None,
                telegram: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_to_localhost() {
        let config = Config::from_str("").unwrap();
        assert_eq!(config.server.bind, "127.0.0.1");
    }

    #[test]
    fn defaults_shell_to_bash() {
        let config = Config::from_str("").unwrap();
        assert_eq!(config.server.shell, "/bin/bash");
    }

    #[test]
    fn defaults_no_auth_token() {
        let config = Config::from_str("").unwrap();
        assert!(config.auth.token.is_none());
    }

    #[test]
    fn parses_auth_token() {
        let config = Config::from_str(
            r#"
            [auth]
            token = "secret123"
            "#,
        )
        .unwrap();
        assert_eq!(config.auth.token.as_deref(), Some("secret123"));
    }

    #[test]
    fn parses_full_config() {
        let config = Config::from_str(
            r#"
            [server]
            bind = "0.0.0.0"
            shell = "/bin/zsh"
            scrollback = 500

            [auth]
            token = "mytoken"

            [transport.rest]
            port = 8080

            [transport.websocket]
            port = 8081

            [transport.mqtt]
            broker = "10.0.0.1"
            port = 1883
            username = "user"
            password = "pass"

            [transport.tcp]
            port = 9999
            "#,
        )
        .unwrap();
        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.server.shell, "/bin/zsh");
        assert_eq!(config.server.scrollback, 500);
        assert_eq!(config.transport.rest.unwrap().port, 8080);
        assert_eq!(config.transport.websocket.unwrap().port, 8081);
        let mqtt = config.transport.mqtt.unwrap();
        assert_eq!(mqtt.broker, "10.0.0.1");
        assert_eq!(mqtt.username.as_deref(), Some("user"));
        assert_eq!(config.transport.tcp.unwrap().port, 9999);
    }

    #[test]
    fn transports_optional() {
        let config = Config::from_str("[server]\nshell = \"/bin/sh\"").unwrap();
        assert!(config.transport.rest.is_none());
        assert!(config.transport.websocket.is_none());
        assert!(config.transport.mqtt.is_none());
        assert!(config.transport.tcp.is_none());
    }

    #[test]
    fn default_ports() {
        let config = Config::from_str(
            r#"
            [transport.rest]
            [transport.websocket]
            [transport.tcp]
            "#,
        )
        .unwrap();
        assert_eq!(config.transport.rest.unwrap().port, 7777);
        assert_eq!(config.transport.websocket.unwrap().port, 7778);
        assert_eq!(config.transport.tcp.unwrap().port, 7779);
    }
}
