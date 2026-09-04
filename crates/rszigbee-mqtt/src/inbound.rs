//! Parsing what arrives from the broker.

use std::time::Duration;

use rszigbee_core::capability::CapabilityId;
use rszigbee_core::state::{StateChanges, StateValue};
use rszigbee_spec::ids::Ieee;
use serde_json::Value;

use crate::topics::Topics;

/// A message off the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The topic it arrived on.
    pub topic: String,
    /// Its body.
    pub payload: Vec<u8>,
}

/// What an inbound message is asking for.
///
/// An *intent*, not a command: this crate does not touch the runtime, so it
/// says what was asked and leaves acting on it — and refusing it — to the
/// caller that owns a [`Zigbee`] handle.
///
/// [`Zigbee`]: rszigbee_core::runtime::Zigbee
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Inbound {
    /// Apply state to a device.
    Set {
        /// Which device.
        ieee: Ieee,
        /// The state to apply.
        changes: StateChanges,
    },
    /// Ask a device to report capabilities.
    Get {
        /// Which device.
        ieee: Ieee,
        /// Which capabilities. Empty means everything the caller knows of.
        capabilities: Vec<CapabilityId>,
    },
    /// Open the network for joining.
    PermitJoin {
        /// How long for. Zero closes it.
        duration: Duration,
    },
    /// A `bridge/request` this build does not implement.
    ///
    /// Carried rather than dropped so a gateway can answer with an error
    /// instead of silence. A request that produces no response at all is
    /// indistinguishable from a broker problem, and that is the hardest kind
    /// of failure to diagnose from the other side.
    UnknownRequest {
        /// The request name, e.g. `device/remove`.
        name: String,
    },
}

/// Why a message could not be understood.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum InboundError {
    /// The topic is outside this gateway's namespace.
    #[error("topic {0} is not in this gateway's namespace")]
    ForeignTopic(String),
    /// The topic is ours but names nothing actionable.
    #[error("topic {0} is not one this gateway reads")]
    UnknownTopic(String),
    /// The device part of the topic is not an address.
    #[error("{0} is not an IEEE address")]
    NotAnAddress(String),
    /// The body is not JSON.
    #[error("payload is not valid JSON: {0}")]
    NotJson(String),
    /// The body is JSON but not the shape this topic needs.
    #[error("payload is not the expected shape: {0}")]
    WrongShape(String),
}

/// Parses one message into an intent.
///
/// # Errors
///
/// Fails rather than guessing. A `/set` whose payload is not an object, or a
/// topic whose device part is not an address, is a caller mistake — and acting
/// on a guess would mean commanding a device nobody named.
pub fn parse(topics: &Topics, message: &Message) -> Result<Inbound, InboundError> {
    let rest = topics
        .strip(&message.topic)
        .ok_or_else(|| InboundError::ForeignTopic(message.topic.clone()))?;

    if let Some(name) = rest.strip_prefix("bridge/request/") {
        return parse_request(name, &message.payload);
    }

    let (address, verb) = rest
        .rsplit_once('/')
        .ok_or_else(|| InboundError::UnknownTopic(message.topic.clone()))?;
    let ieee: Ieee = address
        .parse()
        .map_err(|_| InboundError::NotAnAddress(address.to_owned()))?;

    match verb {
        "set" => Ok(Inbound::Set {
            ieee,
            changes: parse_state(&message.payload)?,
        }),
        "get" => Ok(Inbound::Get {
            ieee,
            capabilities: parse_capabilities(&message.payload)?,
        }),
        _ => Err(InboundError::UnknownTopic(message.topic.clone())),
    }
}

/// A `bridge/request/<name>` body.
fn parse_request(name: &str, payload: &[u8]) -> Result<Inbound, InboundError> {
    if name != "permit_join" {
        return Ok(Inbound::UnknownRequest {
            name: name.to_owned(),
        });
    }
    let value = json(payload)?;
    // Observed as `{"time": 254}`. A bare number and a bare boolean are
    // accepted too: both appear in the wild from hand-written clients, and
    // rejecting them would be stricter than the interface being replaced.
    let seconds = match &value {
        Value::Object(map) => match map.get("time") {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            // `{"value": true}` and `{"time": true}` both mean "open it".
            Some(Value::Bool(true)) | None => 254,
            Some(Value::Bool(false)) => 0,
            Some(other) => {
                return Err(InboundError::WrongShape(format!(
                    "permit_join time must be a number or a boolean, got {other}"
                )));
            }
        },
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::Bool(true) => 254,
        Value::Bool(false) => 0,
        other => {
            return Err(InboundError::WrongShape(format!(
                "permit_join takes an object, a number or a boolean, got {other}"
            )));
        }
    };
    Ok(Inbound::PermitJoin {
        duration: Duration::from_secs(seconds),
    })
}

/// A `/set` body: capability names to desired values.
fn parse_state(payload: &[u8]) -> Result<StateChanges, InboundError> {
    let Value::Object(map) = json(payload)? else {
        return Err(InboundError::WrongShape(
            "a /set payload is an object of capability names to values".to_owned(),
        ));
    };
    let mut changes = StateChanges::new();
    for (key, value) in map {
        changes.set(key.as_str(), from_json(&value));
    }
    Ok(changes)
}

/// A `/get` body: which capabilities to read.
///
/// Observed usage is `{"state": ""}` — the keys name what to read and the
/// values are ignored. An empty object asks for everything.
fn parse_capabilities(payload: &[u8]) -> Result<Vec<CapabilityId>, InboundError> {
    // An empty body is a legitimate "everything", and is what a shell client
    // sends when it publishes with no message.
    if payload.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let Value::Object(map) = json(payload)? else {
        return Err(InboundError::WrongShape(
            "a /get payload is an object whose keys name capabilities".to_owned(),
        ));
    };
    Ok(map.keys().map(|k| CapabilityId::from(k.as_str())).collect())
}

/// Parses a body as JSON.
fn json(payload: &[u8]) -> Result<Value, InboundError> {
    serde_json::from_slice(payload).map_err(|e| InboundError::NotJson(e.to_string()))
}

/// One JSON value as a `StateValue`.
///
/// A string stays a string rather than becoming an `Enum`: which of the two it
/// is depends on the capability, which this layer does not know, and the
/// runtime resolves against the definition anyway.
fn from_json(value: &Value) -> StateValue {
    match value {
        Value::Bool(b) => StateValue::Bool(*b),
        Value::Number(n) => n.as_i64().map_or_else(
            || n.as_f64().map_or(StateValue::Null, StateValue::Float),
            StateValue::Int,
        ),
        Value::String(s) => StateValue::Str(s.clone()),
        Value::Array(items) => StateValue::List(items.iter().map(from_json).collect()),
        Value::Object(map) => {
            StateValue::Map(map.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
        Value::Null => StateValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: Ieee = Ieee::new(0xa4c1_3814_2d62_ffff);

    fn message(topic: &str, payload: &str) -> Message {
        Message {
            topic: topic.to_owned(),
            payload: payload.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_set_message_that_was_confirmed_against_hardware() {
        // This exact topic and payload were published to a running
        // Zigbee2MQTT and opened the valve; `{"state":"OFF"}` closed it again.
        // So this is the observed contract, not an assumed one.
        let topics = Topics::default();
        let parsed = parse(
            &topics,
            &message("zigbee2mqtt/0xa4c138142d62ffff/set", r#"{"state":"ON"}"#),
        )
        .expect("parses");

        let Inbound::Set { ieee, changes } = parsed else {
            panic!("expected a Set, got {parsed:?}");
        };
        assert_eq!(ieee, DEVICE);
        assert_eq!(
            changes.get(&CapabilityId::from("state")),
            Some(&StateValue::Str("ON".into()))
        );
    }

    #[test]
    fn a_get_names_capabilities_by_key() {
        let topics = Topics::default();
        let parsed = parse(
            &topics,
            &message("zigbee2mqtt/0xa4c138142d62ffff/get", r#"{"state":""}"#),
        )
        .expect("parses");
        assert_eq!(
            parsed,
            Inbound::Get {
                ieee: DEVICE,
                capabilities: vec![CapabilityId::from("state")],
            }
        );
    }

    #[test]
    fn an_empty_get_asks_for_everything() {
        // What a shell client sends when it publishes with no message, and it
        // must not be an error.
        let topics = Topics::default();
        let parsed = parse(&topics, &message("zigbee2mqtt/0xa4c138142d62ffff/get", ""))
            .expect("an empty get is legitimate");
        assert_eq!(
            parsed,
            Inbound::Get {
                ieee: DEVICE,
                capabilities: Vec::new(),
            }
        );
    }

    #[test]
    fn permit_join_accepts_the_captured_form_and_the_hand_written_ones() {
        let topics = Topics::default();
        let parse_it = |body: &str| {
            parse(
                &topics,
                &message("zigbee2mqtt/bridge/request/permit_join", body),
            )
            .expect("parses")
        };

        // The observed form.
        assert_eq!(
            parse_it(r#"{"time":254}"#),
            Inbound::PermitJoin {
                duration: Duration::from_secs(254)
            }
        );
        // Forms a hand-written client sends. Accepted because rejecting them
        // would be stricter than the interface being replaced.
        assert_eq!(
            parse_it("true"),
            Inbound::PermitJoin {
                duration: Duration::from_secs(254)
            }
        );
        assert_eq!(
            parse_it("false"),
            Inbound::PermitJoin {
                duration: Duration::ZERO
            }
        );
        assert_eq!(
            parse_it("60"),
            Inbound::PermitJoin {
                duration: Duration::from_secs(60)
            }
        );
    }

    #[test]
    fn an_unimplemented_bridge_request_is_carried_not_dropped() {
        // So the gateway can answer with an error. A request that produces no
        // response is indistinguishable from a broker problem from the other
        // side, which is the hardest failure to diagnose.
        let topics = Topics::default();
        let parsed = parse(
            &topics,
            &message("zigbee2mqtt/bridge/request/device/remove", r#"{"id":"x"}"#),
        )
        .expect("parses");
        assert_eq!(
            parsed,
            Inbound::UnknownRequest {
                name: "device/remove".into()
            }
        );
    }

    #[test]
    fn messages_that_are_not_ours_are_refused() {
        let topics = Topics::default();

        // Another namespace that starts with ours as a string.
        assert!(matches!(
            parse(&topics, &message("zigbee2mqtt-test/0x1/set", "{}")),
            Err(InboundError::ForeignTopic(_))
        ));
        // Our own publish, coming back. Acting on this would feed the gateway
        // its own state as a command.
        assert!(matches!(
            parse(
                &topics,
                &message("zigbee2mqtt/0xa4c138142d62ffff", r#"{"state":"ON"}"#)
            ),
            Err(InboundError::UnknownTopic(_))
        ));
        // A device part that is not an address.
        assert!(matches!(
            parse(&topics, &message("zigbee2mqtt/kitchen-lamp/set", "{}")),
            Err(InboundError::NotAnAddress(_))
        ));
    }

    #[test]
    fn a_malformed_payload_is_an_error_and_not_a_guess() {
        let topics = Topics::default();
        assert!(matches!(
            parse(
                &topics,
                &message("zigbee2mqtt/0xa4c138142d62ffff/set", "not json")
            ),
            Err(InboundError::NotJson(_))
        ));
        // Valid JSON, wrong shape. Guessing here would command a device from a
        // payload nobody meant as a command.
        assert!(matches!(
            parse(
                &topics,
                &message("zigbee2mqtt/0xa4c138142d62ffff/set", r#"["state"]"#)
            ),
            Err(InboundError::WrongShape(_))
        ));
    }
}
