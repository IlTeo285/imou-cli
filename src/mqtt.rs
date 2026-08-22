use std::time::Duration;

use rumqttc::{AsyncClient, MqttOptions, QoS, Transport};

use crate::motion_event::{MotionAnalysisEvent, MotionEvent};

/// Broker connection details plus the topic-naming convention (one topic
/// per camera: `{topic_prefix}/<channel_name>/motion`). Presence of
/// `MQTT_BROKER_HOST`/`MQTT_BROKER_PORT` in the environment is what gates
/// the whole feature — same "optional, no-op if unset" convention as
/// `gdrive::GDriveConfig`.
pub struct MqttConfig {
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    tls: bool,
    topic_prefix: String,
    client_id: String,
}

/// Reads `MQTT_BROKER_HOST`/`MQTT_BROKER_PORT` (both required together) plus
/// optional `MQTT_USERNAME`/`MQTT_PASSWORD`/`MQTT_TLS`/`MQTT_TOPIC_PREFIX`/
/// `MQTT_CLIENT_ID` from the environment. Returns `None` if the required
/// pair is missing — callers should treat that as "MQTT publish not
/// configured," not as an error.
pub fn config_from_env() -> Option<MqttConfig> {
    let host = std::env::var("MQTT_BROKER_HOST").ok()?;
    let port = std::env::var("MQTT_BROKER_PORT").ok()?.parse::<u16>().ok()?;

    let username = std::env::var("MQTT_USERNAME").ok();
    let password = std::env::var("MQTT_PASSWORD").ok();
    let tls = matches!(std::env::var("MQTT_TLS").as_deref(), Ok("true") | Ok("1"));
    let topic_prefix = std::env::var("MQTT_TOPIC_PREFIX").unwrap_or_else(|_| "imou".to_string());
    let client_id = std::env::var("MQTT_CLIENT_ID")
        .unwrap_or_else(|_| format!("imou-cli-{}", uuid::Uuid::new_v4()));

    Some(MqttConfig {
        host,
        port,
        username,
        password,
        tls,
        topic_prefix,
        client_id,
    })
}

/// Publishes each `MotionEvent` to the broker as fire-and-forget — a slow
/// or unreachable broker must never block `watch`'s poll loop or `listen`'s
/// push HTTP handler, same posture as Google Drive clip upload.
pub struct MqttPublisher {
    client: AsyncClient,
    pub host: String,
    pub port: u16,
    pub topic_prefix: String,
}

impl MqttPublisher {
    /// Builds the client and spawns the background task that drives its
    /// event loop. Deliberately infallible and not `async`: `MqttOptions`/
    /// `AsyncClient::new` do no I/O themselves — the real TCP/TLS connect
    /// only happens lazily once the event loop is polled — so a broker
    /// being unreachable never delays `watch::run`/`listen::run` startup.
    pub fn connect(config: MqttConfig) -> Self {
        let mut options = MqttOptions::new(config.client_id, config.host.clone(), config.port);
        options.set_keep_alive(Duration::from_secs(30));

        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            options.set_credentials(username, password);
        }
        if config.tls {
            options.set_transport(Transport::tls_with_default_config());
        }

        let (client, mut eventloop) = AsyncClient::new(options, 10);

        // The initial connect timeout is a *separate* setting from
        // `keep_alive` above, and lives on `EventLoop`, not `MqttOptions` —
        // easy to miss. Same explicit-timeout rule as `ImouClient::new`
        // and `GDriveClient::new`: this codebase has a documented incident
        // where a network call with no timeout silently hung a long-lived
        // loop forever.
        eventloop.network_options.set_connection_timeout(10);

        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("warning: mqtt connection error: {e}");
                        // rumqttc reconnects automatically as long as poll()
                        // keeps being called, but documents no built-in
                        // backoff — without this delay a broker that's down
                        // for a while would be hammered in a hot spin.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        Self {
            client,
            host: config.host,
            port: config.port,
            topic_prefix: config.topic_prefix,
        }
    }

    /// Fire-and-forget: spawns its own task rather than awaiting the
    /// publish inline, because rumqttc's `publish().await` can itself
    /// block on internal backpressure if the event loop isn't keeping up
    /// (broker down/slow) — the same "one stalled network call hangs the
    /// loop" failure class this project already hit once with `reqwest`.
    pub fn publish(&self, event: &MotionEvent, channel_name: &str) {
        let topic = format!("{}/{}/motion", self.topic_prefix, channel_name);
        self.publish_to(topic, event);
    }

    /// Same fire-and-forget posture as `publish`, on a separate topic since
    /// this fires later — once AI analysis of the extracted clip completes
    /// (see `MotionAnalysisEvent`'s doc note on why it's a deferred, second
    /// event rather than folded into the immediate `motion` publish).
    pub fn publish_analysis(&self, event: &MotionAnalysisEvent, channel_name: &str) {
        let topic = format!("{}/{}/motion-analyzed", self.topic_prefix, channel_name);
        self.publish_to(topic, event);
    }

    fn publish_to(&self, topic: String, event: &impl serde::Serialize) {
        let payload = match serde_json::to_vec(event) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("warning: failed to serialize motion event for mqtt: {e}");
                return;
            }
        };

        let client = self.client.clone();
        tokio::spawn(async move {
            // At-least-once, not retained: this is an event stream, not a
            // state topic — a retained "last motion" value would
            // misrepresent old motion as fresh to a newly-connecting
            // subscriber.
            if let Err(e) = client.publish(topic, QoS::AtLeastOnce, false, payload).await {
                eprintln!("warning: mqtt publish failed: {e}");
            }
        });
    }
}
