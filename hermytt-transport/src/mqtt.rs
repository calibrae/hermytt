use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hermytt_core::SessionManager;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
use tracing::{error, info, warn};

use crate::Transport;

const SILENCE_TIMEOUT: Duration = Duration::from_millis(500);

pub struct MqttTransport {
    pub broker_host: String,
    pub broker_port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Parse an MQTT topic like `hermytt/{session_id}/in`.
/// Returns the session ID if valid, `None` otherwise.
pub fn parse_input_topic(topic: &str) -> Option<&str> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() != 3 || parts[0] != "hermytt" || parts[2] != "in" {
        return None;
    }
    let session_id = parts[1];
    if session_id == "default" || session_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(session_id)
    } else {
        None
    }
}

#[async_trait]
impl Transport for MqttTransport {
    async fn serve(self: Arc<Self>, sessions: Arc<SessionManager>) -> Result<()> {
        let client_id = format!("hermytt-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let mut opts = MqttOptions::new(&client_id, &self.broker_host, self.broker_port);
        opts.set_keep_alive(Duration::from_secs(30));

        if let (Some(user), Some(pass)) = (&self.username, &self.password) {
            opts.set_credentials(user, pass);
        }

        let (client, eventloop) = AsyncClient::new(opts, 64);

        client
            .subscribe("hermytt/+/in", QoS::AtMostOnce)
            .await
            .context("failed to subscribe to hermytt/+/in")?;

        info!(
            transport = "mqtt",
            broker = %format!("{}:{}", self.broker_host, self.broker_port),
            "connected"
        );

        request_loop(eventloop, client, sessions).await;

        Ok(())
    }

    fn name(&self) -> &str {
        "mqtt"
    }
}

/// Receive commands on /in, execute, publish response on /out.
async fn request_loop(
    mut eventloop: EventLoop,
    client: AsyncClient,
    sessions: Arc<SessionManager>,
) {
    loop {
        match eventloop.poll().await {
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                let Some(session_id) = parse_input_topic(&publish.topic) else {
                    continue;
                };

                let handle = if session_id == "default" {
                    sessions.default_session().await.ok()
                } else {
                    sessions.get_session(session_id).await
                };

                let Some(handle) = handle else {
                    warn!(topic = %publish.topic, "message for unknown session");
                    continue;
                };

                let cmd = String::from_utf8_lossy(&publish.payload).to_string();
                let out_topic = publish.topic.replace("/in", "/out");

                // Execute and publish response.
                let client = client.clone();
                let handle = handle.clone();
                tokio::spawn(async move {
                    match handle.execute(&cmd, SILENCE_TIMEOUT).await {
                        Ok(data) => {
                            let raw = String::from_utf8_lossy(&data);
                            let clean = crate::telegram::clean_output(&raw, &cmd);
                            if let Err(e) = client
                                .publish(&out_topic, QoS::AtMostOnce, false, clean.as_bytes())
                                .await
                            {
                                error!(error = %e, topic = %out_topic, "publish failed");
                            }
                        }
                        Err(e) => {
                            error!(error = %e, "execute failed");
                        }
                    }
                });
            }
            Ok(_) => {}
            Err(e) => {
                error!(error = %e, "mqtt connection error, retrying...");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_topic() {
        assert_eq!(parse_input_topic("hermytt/abc123/in"), Some("abc123"));
    }

    #[test]
    fn parse_default_topic() {
        assert_eq!(parse_input_topic("hermytt/default/in"), Some("default"));
    }

    #[test]
    fn parse_rejects_wrong_prefix() {
        assert_eq!(parse_input_topic("other/abc123/in"), None);
    }

    #[test]
    fn parse_rejects_wrong_suffix() {
        assert_eq!(parse_input_topic("hermytt/abc123/out"), None);
    }

    #[test]
    fn parse_rejects_too_few_segments() {
        assert_eq!(parse_input_topic("hermytt/in"), None);
    }

    #[test]
    fn parse_rejects_too_many_segments() {
        assert_eq!(parse_input_topic("hermytt/a/b/in"), None);
    }

    #[test]
    fn parse_rejects_traversal() {
        assert_eq!(parse_input_topic("hermytt/../../in"), None);
    }

    #[test]
    fn parse_rejects_special_chars() {
        assert_eq!(parse_input_topic("hermytt/abc+def/in"), None);
        assert_eq!(parse_input_topic("hermytt/abc#def/in"), None);
    }
}
