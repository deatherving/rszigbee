//! The MQTT gateway: an rszigbee runtime driven from a broker.
//!
//! [`rszigbee_mqtt`] holds the contract and no client, so it can be tested
//! against captured payloads without a broker. This crate is the other half —
//! the client, the connection, and the loop that joins a runtime to it.
//!
//! The split is deliberate. Everything that has to be byte-for-byte right
//! lives in the sans-IO crate and is tested there; what lives here is
//! plumbing, and plumbing is the part that needs a broker to exercise.
//!
//! # What it does
//!
//! ```text
//! runtime events  ──►  translate  ──►  publish
//! broker messages ──►  parse      ──►  act on the runtime  ──►  respond
//! ```
//!
//! Availability is a **will**, not a shutdown message. The broker publishes
//! `offline` on our behalf if the connection drops without a clean disconnect,
//! which is the case that matters: a gateway that only announced `offline` on a
//! graceful stop looks online forever after a crash.

#![forbid(unsafe_code)]

use std::time::Duration;

use rszigbee_core::command::DeviceCommand;
use rszigbee_core::runtime::Zigbee;
use rszigbee_mqtt::{Inbound, InboundError, Message, Publication, StateStore, Topics, parse};
use rumqttc::{AsyncClient, LastWill, MqttOptions, QoS};
use serde_json::json;
use tracing::{debug, info, warn};

/// How to reach the broker, and what to publish under.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Broker hostname or address.
    pub host: String,
    /// Broker port.
    pub port: u16,
    /// MQTT client id.
    pub client_id: String,
    /// Optional username and password.
    pub credentials: Option<(String, String)>,
    /// The topic namespace.
    pub topics: Topics,
    /// Keep-alive interval.
    pub keep_alive: Duration,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_owned(),
            port: 1883,
            client_id: "rszigbee".to_owned(),
            credentials: None,
            topics: Topics::default(),
            // The value a reference gateway uses, so a broker configured for
            // one behaves the same for this.
            keep_alive: Duration::from_secs(60),
        }
    }
}

/// Why the gateway stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// The broker connection failed and did not recover.
    #[error("MQTT connection failed: {0}")]
    Mqtt(#[from] rumqttc::ClientError),
    /// The runtime stopped.
    #[error("the Zigbee runtime stopped")]
    RuntimeStopped,
}

/// Runs until the runtime stops or the broker connection fails terminally.
///
/// Takes the [`Zigbee`] handle by reference: the runtime outlives the gateway,
/// and a caller may well want to keep using the typed API alongside it. The
/// MQTT mode is an addition to the library, not a replacement for it.
///
/// # Errors
///
/// Fails if the broker rejects the initial subscribe or publish, or if the
/// runtime's event stream closes.
pub async fn run(zigbee: &Zigbee, config: GatewayConfig) -> Result<(), GatewayError> {
    let mut store = StateStore::new(config.topics.clone());

    let mut options = MqttOptions::new(&config.client_id, &config.host, config.port);
    options.set_keep_alive(config.keep_alive);
    if let Some((user, password)) = &config.credentials {
        options.set_credentials(user, password);
    }
    // Set before connecting, because a will can only be registered as part of
    // the CONNECT packet. Registering it afterwards is not possible, which is
    // the trap: it looks like ordinary configuration and silently does nothing.
    let offline = store.offline();
    options.set_last_will(LastWill::new(
        offline.topic.clone(),
        offline.payload.clone(),
        QoS::AtLeastOnce,
        offline.retain,
    ));

    let (client, mut eventloop) = AsyncClient::new(options, 64);

    for filter in config.topics.subscriptions() {
        client.subscribe(&filter, QoS::AtLeastOnce).await?;
        debug!(%filter, "subscribed");
    }
    publish(&client, &store.online()).await?;
    info!(
        broker = %format_args!("{}:{}", config.host, config.port),
        base = config.topics.base(),
        "MQTT gateway online"
    );

    let mut events = zigbee.events();

    loop {
        tokio::select! {
            // Runtime side: whatever happened, published.
            event = events.recv() => {
                let Some(event) = event else {
                    // A clean runtime stop. Said explicitly rather than left to
                    // the will, so a graceful shutdown does not look like a
                    // crash to everything subscribed.
                    let _ = publish(&client, &store.offline()).await;
                    let _ = client.disconnect().await;
                    return Err(GatewayError::RuntimeStopped);
                };
                for publication in store.translate(&event) {
                    if let Err(e) = publish(&client, &publication).await {
                        warn!(topic = %publication.topic, error = %e, "could not publish");
                    }
                }
            }

            // Broker side.
            polled = eventloop.poll() => match polled {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(message))) => {
                    handle(zigbee, &client, &store, &config.topics, &message).await;
                }
                Ok(_) => {}
                Err(e) => {
                    // Not fatal. rumqttc reconnects on its own, and treating a
                    // dropped connection as the end would make the gateway
                    // quit the first time the broker restarts.
                    warn!(error = %e, "MQTT connection problem; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            },
        }
    }
}

/// Publishes one translation result.
async fn publish(
    client: &AsyncClient,
    publication: &Publication,
) -> Result<(), rumqttc::ClientError> {
    client
        .publish(
            &publication.topic,
            QoS::AtLeastOnce,
            publication.retain,
            publication.payload.as_bytes(),
        )
        .await
}

/// Acts on one inbound message.
///
/// Every failure is logged and answered where the contract has somewhere to
/// answer, never propagated: a malformed message from the broker is not a
/// reason for the gateway to stop, and the sender is the one who needs to know.
async fn handle(
    zigbee: &Zigbee,
    client: &AsyncClient,
    store: &StateStore,
    topics: &Topics,
    message: &rumqttc::Publish,
) {
    let topic = message.topic.clone();
    let parsed = parse(
        topics,
        &Message {
            topic: topic.clone(),
            payload: message.payload.to_vec(),
        },
    );

    let intent = match parsed {
        Ok(intent) => intent,
        // A foreign topic is not worth a warning: a broker with overlapping
        // subscriptions delivers them routinely and it is not our business.
        Err(InboundError::ForeignTopic(_)) => return,
        Err(e) => {
            warn!(%topic, error = %e, "ignoring an inbound message");
            return;
        }
    };

    match intent {
        Inbound::Set { ieee, changes } => {
            match zigbee.send(ieee, DeviceCommand::Set(changes)).await {
                Ok(outcome) => debug!(%ieee, ?outcome, "applied a /set"),
                Err(e) => warn!(%ieee, error = %e, "a /set was refused"),
            }
        }
        Inbound::Get { ieee, capabilities } => {
            match zigbee.send(ieee, DeviceCommand::Get(capabilities)).await {
                Ok(outcome) => debug!(%ieee, ?outcome, "applied a /get"),
                Err(e) => warn!(%ieee, error = %e, "a /get was refused"),
            }
        }
        Inbound::PermitJoin { duration } => {
            let result = zigbee.permit_join(duration, None).await;
            // Answered either way. The response is how a caller learns it
            // worked, and silence is indistinguishable from a broker problem.
            let response = store.response(
                "permit_join",
                &json!({"time": duration.as_secs()}),
                result.is_ok(),
            );
            if let Err(e) = &result {
                warn!(error = %e, "permit join was refused");
            }
            let _ = publish(client, &response).await;
        }
        Inbound::UnknownRequest { name } => {
            // Answered with an error rather than dropped, for the same reason.
            warn!(%name, "unimplemented bridge request");
            let response = store.response(
                &name,
                &json!({"error": "this gateway does not implement that request"}),
                false,
            );
            let _ = publish(client, &response).await;
        }
        // `Inbound` is `#[non_exhaustive]`; a new intent is ignored rather than
        // guessed at.
        _ => debug!(%topic, "an inbound intent this build does not act on"),
    }
}
