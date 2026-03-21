use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hermytt_core::SessionManager;
use hermytt_transport::Transport;
use hermytt_transport::mqtt::MqttTransport;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use rumqttd::{Broker, Config, ConnectionSettings, RouterConfig, ServerSettings};

/// Start an embedded MQTT broker on a random port, return the port.
fn start_broker() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    let mut v4 = HashMap::new();
    v4.insert(
        "test".to_string(),
        ServerSettings {
            name: "test".to_string(),
            listen: addr,
            tls: None,
            next_connection_delay_ms: 1,
            connections: ConnectionSettings {
                connection_timeout_ms: 5000,
                max_payload_size: 65536,
                max_inflight_count: 100,
                auth: None,
                external_auth: None,
                dynamic_filters: true,
            },
        },
    );

    let config = Config {
        id: 0,
        router: RouterConfig {
            max_connections: 100,
            max_outgoing_packet_count: 200,
            max_segment_size: 1048576,
            max_segment_count: 10,
            custom_segment: None,
            initialized_filters: None,
            shared_subscriptions_strategy: Default::default(),
        },
        v4: Some(v4),
        v5: None,
        ws: None,
        cluster: None,
        console: None,
        bridge: None,
        prometheus: None,
        metrics: None,
    };

    let mut broker = Broker::new(config);
    std::thread::spawn(move || {
        broker.start().unwrap();
    });

    // Wait for broker to be ready.
    for _ in 0..50 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("broker didn't start");
}

#[tokio::test]
async fn mqtt_exec_and_response() {
    let broker_port = start_broker();

    // Start hermytt MQTT transport.
    let sessions = Arc::new(SessionManager::new("/bin/sh", 100));
    sessions.create_session().await.unwrap();

    let transport = Arc::new(MqttTransport {
        broker_host: "127.0.0.1".to_string(),
        broker_port,
        username: None,
        password: None,
    });

    let sessions_clone = sessions.clone();
    tokio::spawn(async move {
        transport.serve(sessions_clone).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect a test client.
    let mut opts = MqttOptions::new("test-client", "127.0.0.1", broker_port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 10);

    // Spawn event loop.
    let event_handle = tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    // Subscribe to output.
    client
        .subscribe("hermytt/default/out", QoS::AtMostOnce)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Send a command.
    client
        .publish("hermytt/default/in", QoS::AtMostOnce, false, "echo mqtt-test-123")
        .await
        .unwrap();

    // Wait for the response (poll for up to 5s).
    let mut got_response = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    // Reconnect a second subscriber to read the output.
    let mut opts2 = MqttOptions::new("test-reader", "127.0.0.1", broker_port);
    opts2.set_keep_alive(Duration::from_secs(5));
    let (client2, mut eventloop2) = AsyncClient::new(opts2, 10);

    client2
        .subscribe("hermytt/default/out", QoS::AtMostOnce)
        .await
        .unwrap();

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), eventloop2.poll()).await {
            Ok(Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)))) => {
                let payload = String::from_utf8_lossy(&publish.payload);
                if payload.contains("mqtt-test-123") {
                    got_response = true;
                    break;
                }
            }
            _ => {
                // Send again in case the first one was too early.
                let _ = client
                    .publish(
                        "hermytt/default/in",
                        QoS::AtMostOnce,
                        false,
                        "echo mqtt-test-123",
                    )
                    .await;
            }
        }
    }

    assert!(got_response, "did not receive MQTT response");

    event_handle.abort();
}

#[tokio::test]
async fn mqtt_topic_routing() {
    let broker_port = start_broker();

    let sessions = Arc::new(SessionManager::new("/bin/sh", 100));
    sessions.create_session().await.unwrap();

    let transport = Arc::new(MqttTransport {
        broker_host: "127.0.0.1".to_string(),
        broker_port,
        username: None,
        password: None,
    });

    let sessions_clone = sessions.clone();
    tokio::spawn(async move {
        transport.serve(sessions_clone).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Invalid topic should be ignored (no crash).
    let mut opts = MqttOptions::new("test-bad-topic", "127.0.0.1", broker_port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 10);

    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    // These should be silently ignored by hermytt's topic parser.
    // Note: MQTT wildcards (+, #) are rejected by the client itself.
    client
        .publish("other/default/in", QoS::AtMostOnce, false, "whoami")
        .await
        .unwrap();
    client
        .publish("hermytt/default/out", QoS::AtMostOnce, false, "whoami")
        .await
        .unwrap();
    client
        .publish("hermytt/nonexistent/in", QoS::AtMostOnce, false, "whoami")
        .await
        .unwrap();

    // No crash after 1s = success.
    tokio::time::sleep(Duration::from_secs(1)).await;
}
