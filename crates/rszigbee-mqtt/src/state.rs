//! Turning runtime events into MQTT publications.

use std::collections::BTreeMap;

use rszigbee_core::event::Event;
use rszigbee_core::state::{StateChanges, StateValue};
use rszigbee_spec::ids::Ieee;
use serde_json::{Map, Value, json};

use crate::topics::Topics;

/// One thing to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// Where it goes.
    pub topic: String,
    /// The JSON body, already serialised.
    pub payload: String,
    /// Whether the broker should keep it for late subscribers.
    ///
    /// Only the bridge's own availability is retained. Retaining device state
    /// would hand a new subscriber a reading with no indication of its age,
    /// which for a sensor is worse than no reading.
    pub retain: bool,
}

/// A device's accumulated state.
///
/// `Zigbee2MQTT` publishes a device's *whole* known state on every change, so a
/// consumer that reads one message gets a complete picture. Publishing only the
/// changed field would look right in a log and break every consumer that does
/// not accumulate — which is most of them, including Home Assistant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceState {
    fields: BTreeMap<String, Value>,
}

impl DeviceState {
    /// Applies a delta and returns the whole state as JSON.
    ///
    /// `BTreeMap` so the key order is stable. Not cosmetic: a payload whose
    /// field order changes between publishes defeats every diff and every
    /// change-detecting subscriber, and makes captured payloads impossible to
    /// compare in a test.
    pub fn apply(&mut self, changes: &StateChanges) -> String {
        for (id, value) in changes.iter() {
            let name = id.as_str();
            // The capability name is needed, not just the value: whether a
            // boolean publishes as `true` or as `"ON"` depends on which
            // capability it is.
            let rendered = match (value, boolean_rendering(name)) {
                (StateValue::Bool(b), Some((on, off))) => {
                    json!(if *b { on } else { off })
                }
                _ => to_json(value),
            };
            self.fields.insert(name.to_owned(), rendered);
        }
        let map: Map<String, Value> = self.fields.clone().into_iter().collect();
        Value::Object(map).to_string()
    }

    /// The fields known so far.
    #[must_use]
    pub fn fields(&self) -> &BTreeMap<String, Value> {
        &self.fields
    }
}

/// How a boolean capability renders, for the capabilities where a bare
/// `true`/`false` is not what the contract publishes.
///
/// Captured, not invented: the reference publishes `"state":"OFF"` and
/// `"child_lock":"UNLOCK"`, never `false`. A consumer that switches on the
/// string -- Home Assistant's MQTT switch does exactly that -- sees nothing it
/// recognises in a boolean.
///
/// Upstream derives these from the definition's `value_on` and `value_off`,
/// which this layer cannot see: it is given capability names and values, not
/// definitions. A named table of the observed pairs is the honest version of
/// that, and an unlisted capability keeps its boolean rather than being given a
/// guessed spelling.
const BOOLEAN_RENDERINGS: &[(&str, &str, &str)] = &[
    ("state", "ON", "OFF"),
    ("child_lock", "LOCK", "UNLOCK"),
    ("window_open", "OPEN", "CLOSE"),
    // No entry for `occupancy` and friends on purpose. A pair of "true"/"false"
    // here would publish the *string* `"true"`, which is neither the boolean a
    // consumer expects nor a word it switches on -- strictly worse than
    // leaving it alone.
];

/// The strings a boolean capability renders as, if it has any.
fn boolean_rendering(capability: &str) -> Option<(&'static str, &'static str)> {
    BOOLEAN_RENDERINGS
        .iter()
        .find(|(name, _, _)| *name == capability)
        .map(|(_, on, off)| (*on, *off))
}

/// A float as an integer, when it is exactly one and fits.
///
/// Range-checked before the conversion, which is what makes it exact rather
/// than truncating. A value outside `i64` keeps its float rendering rather
/// than being clamped to something that is not the reading.
fn integral(value: f64) -> Option<i64> {
    #[allow(clippy::cast_possible_truncation)]
    (value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0)
        .then_some(value as i64)
}

/// One `StateValue` as JSON.
///
/// `Enum` becomes a string rather than a tagged object, because that is what
/// was observed: `"child_lock":"UNLOCK"`, not `{"type":"enum","value":…}`.
fn to_json(value: &StateValue) -> Value {
    match value {
        StateValue::Bool(b) => json!(b),
        StateValue::Int(i) => json!(i),
        // An integral float is published as an integer, because that is what
        // was captured: `"battery":100`, not `100.0`. The runtime carries
        // battery percentage as a float since the raw value is halved, and a
        // consumer with strict JSON typing -- or one comparing payloads
        // against a reference gateway's -- sees the difference.
        StateValue::Float(f) => integral(*f).map_or_else(|| json!(f), |i| json!(i)),
        StateValue::Str(s) | StateValue::Enum(s) => json!(s),
        StateValue::List(items) => Value::Array(items.iter().map(to_json).collect()),
        StateValue::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect(),
        ),
        // The explicit null, and anything a future upstream variant adds:
        // `StateValue` is `#[non_exhaustive]`. Null rather than a panic or a
        // stringified debug form, because a capability this build cannot
        // represent should read as absent rather than as a plausible-looking
        // wrong value.
        StateValue::Null | _ => Value::Null,
    }
}

/// Accumulated state for every device, and the event translation over it.
#[derive(Debug, Default)]
pub struct StateStore {
    topics: Topics,
    devices: BTreeMap<Ieee, DeviceState>,
}

impl StateStore {
    /// A store publishing under `topics`.
    #[must_use]
    pub fn new(topics: Topics) -> Self {
        Self {
            topics,
            devices: BTreeMap::new(),
        }
    }

    /// The topics in use.
    #[must_use]
    pub fn topics(&self) -> &Topics {
        &self.topics
    }

    /// What to publish when the gateway comes up, and what to leave as its will.
    ///
    /// Returned as a pair because the offline message is a *will*: the broker
    /// sends it if the connection drops without a clean disconnect, which is
    /// the case that matters. A gateway that only published `offline` on a
    /// graceful shutdown would look online forever after a crash.
    #[must_use]
    pub fn online(&self) -> Publication {
        Publication {
            topic: self.topics.bridge_state(),
            payload: json!({"state": "online"}).to_string(),
            retain: true,
        }
    }

    /// The message a broker should publish on our behalf if we vanish.
    #[must_use]
    pub fn offline(&self) -> Publication {
        Publication {
            topic: self.topics.bridge_state(),
            payload: json!({"state": "offline"}).to_string(),
            retain: true,
        }
    }

    /// What an event becomes on the wire.
    ///
    /// A `Vec` because one event can be several publications and many events
    /// are none at all. Returning an `Option` would force the caller to
    /// distinguish "nothing to say" from "nothing happened", which is not a
    /// distinction that matters here.
    pub fn translate(&mut self, event: &Event) -> Vec<Publication> {
        match event {
            Event::StateChanged { ieee, changes, .. } => {
                let payload = self.devices.entry(*ieee).or_default().apply(changes);
                vec![Publication {
                    topic: self.topics.device(*ieee),
                    payload,
                    // Not retained: a reading with no indication of its age is
                    // worse than none, and a late subscriber cannot tell.
                    retain: false,
                }]
            }
            Event::DeviceJoined { ieee, .. } => vec![self.bridge_event("device_joined", *ieee)],
            Event::DeviceAnnounced { ieee } => {
                vec![self.bridge_event("device_announce", *ieee)]
            }
            // `device_leave`, not `device_left`: the observed spelling.
            Event::DeviceLeft { ieee, .. } => vec![self.bridge_event("device_leave", *ieee)],
            Event::InterviewStarted { ieee } => vec![self.interview_event(*ieee, "started")],
            Event::InterviewFinished { ieee, state } => {
                let status = match state {
                    rszigbee_core::device::InterviewState::Successful => "successful",
                    _ => "failed",
                };
                vec![self.interview_event(*ieee, status)]
            }
            // Everything else has no place in this contract. Listed as a
            // catch-all rather than enumerated, because `Event` is
            // `#[non_exhaustive]` and a new variant must not stop this
            // compiling -- but note that means a new variant is silently
            // unpublished until someone adds it here.
            _ => Vec::new(),
        }
    }

    /// A `bridge/event` naming a device.
    fn bridge_event(&self, kind: &str, ieee: Ieee) -> Publication {
        let address = ieee.to_string();
        Publication {
            topic: self.topics.bridge_event(),
            payload: json!({
                "type": kind,
                "data": {
                    // Both, with the same value. Zigbee2MQTT sends a friendly
                    // name and falls back to the address when none is set;
                    // consumers read one or the other, so omitting either
                    // breaks some of them.
                    "friendly_name": address,
                    "ieee_address": address,
                }
            })
            .to_string(),
            retain: false,
        }
    }

    /// A `bridge/event` of type `device_interview`.
    fn interview_event(&self, ieee: Ieee, status: &str) -> Publication {
        let address = ieee.to_string();
        Publication {
            topic: self.topics.bridge_event(),
            payload: json!({
                "type": "device_interview",
                "data": {
                    "friendly_name": address,
                    "ieee_address": address,
                    "status": status,
                }
            })
            .to_string(),
            retain: false,
        }
    }

    /// The answer to a `bridge/request`.
    #[must_use]
    pub fn response(&self, name: &str, data: &Value, ok: bool) -> Publication {
        Publication {
            topic: self.topics.bridge_response(name),
            payload: json!({"data": data, "status": if ok { "ok" } else { "error" }}).to_string(),
            retain: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use rszigbee_core::state::StateChanges;

    use super::*;

    const DEVICE: Ieee = Ieee::new(0xa4c1_3814_2d62_ffff);

    #[test]
    fn state_accumulates_rather_than_publishing_only_the_change() {
        // The behaviour every consumer depends on, and the one a delta-only
        // gateway would break while looking correct in a log. Captured from a
        // running Zigbee2MQTT driving this exact device: `state` arrived first,
        // then `battery`, and the second publish carried both.
        let mut store = StateStore::new(Topics::default());

        let first = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("state", StateValue::Enum("OFF".into())),
        });
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].topic, "zigbee2mqtt/0xa4c138142d62ffff");
        assert_eq!(first[0].payload, r#"{"state":"OFF"}"#);

        let second = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("battery", StateValue::Int(100)),
        });
        assert_eq!(
            second[0].payload, r#"{"battery":100,"state":"OFF"}"#,
            "the second publish must carry the first field too"
        );
    }

    #[test]
    fn one_devices_state_does_not_leak_into_another() {
        // The bug accumulation invites. Two devices, one store.
        const OTHER: Ieee = Ieee::new(0x0012_4b00_2218_9abc);
        let mut store = StateStore::new(Topics::default());
        store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("state", StateValue::Enum("ON".into())),
        });
        let other = store.translate(&Event::StateChanged {
            ieee: OTHER,
            endpoint: None,
            changes: StateChanges::new().with("battery", StateValue::Int(42)),
        });
        assert_eq!(
            other[0].payload, r#"{"battery":42}"#,
            "a second device must not inherit the first device's state"
        );
        assert_eq!(other[0].topic, "zigbee2mqtt/0x00124b0022189abc");
    }

    #[test]
    fn a_boolean_state_publishes_as_on_or_off() {
        // Found by running the gateway against a broker: we published
        // `"state":true` where the captured reference payload says
        // `"state":"OFF"`. Home Assistant's MQTT switch matches on the string,
        // so a boolean is not something it recognises.
        let mut store = StateStore::new(Topics::default());
        let on = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("state", StateValue::Bool(true)),
        });
        assert_eq!(on[0].payload, r#"{"state":"ON"}"#);

        let off = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("state", StateValue::Bool(false)),
        });
        assert_eq!(off[0].payload, r#"{"state":"OFF"}"#);

        // The other observed one, with different words.
        let locked = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("child_lock", StateValue::Bool(false)),
        });
        assert!(
            locked[0].payload.contains(r#""child_lock":"UNLOCK""#),
            "got {}",
            locked[0].payload
        );
    }

    #[test]
    fn an_unlisted_boolean_capability_keeps_its_boolean() {
        // The control. Giving every boolean a guessed spelling would be worse
        // than leaving it as one: `true` is at least unambiguous, where an
        // invented "ON" for a capability the reference publishes as a boolean
        // is a difference nobody asked for.
        let mut store = StateStore::new(Topics::default());
        let published = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("some_new_flag", StateValue::Bool(true)),
        });
        assert_eq!(published[0].payload, r#"{"some_new_flag":true}"#);

        // And a boolean must never come out as the *string* "true", which is
        // what an over-eager entry in the rendering table would produce: it is
        // neither the boolean a consumer expects nor a word it switches on.
        assert!(
            !published[0].payload.contains(r#""true""#),
            "a boolean must not become a quoted string, got {}",
            published[0].payload
        );
    }

    #[test]
    fn an_integral_float_publishes_as_an_integer() {
        // Found by running the gateway against a broker and diffing what a
        // subscriber saw against the captured reference payloads: ours said
        // `"battery":100.0` where the reference says `"battery":100`. The
        // runtime carries battery as a float because the raw value is halved.
        let mut store = StateStore::new(Topics::default());
        let published = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("battery", StateValue::Float(100.0)),
        });
        assert_eq!(published[0].payload, r#"{"battery":100}"#);

        // And a genuinely fractional value keeps its fraction: this is about
        // matching the reference's rendering, not about rounding readings.
        let published = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("temperature", StateValue::Float(21.37)),
        });
        assert!(
            published[0].payload.contains("21.37"),
            "a fractional reading must not be rounded, got {}",
            published[0].payload
        );
    }

    #[test]
    fn field_order_is_stable() {
        // A payload whose key order moves between publishes defeats every
        // change-detecting subscriber and makes captured payloads impossible
        // to compare. Inserted in a deliberately unhelpful order.
        let mut store = StateStore::new(Topics::default());
        let published = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new()
                .with("state", StateValue::Enum("OFF".into()))
                .with("battery", StateValue::Int(100))
                .with("linkquality", StateValue::Int(255)),
        });
        assert_eq!(
            published[0].payload, r#"{"battery":100,"linkquality":255,"state":"OFF"}"#,
            "keys must be ordered, and match the captured payload's ordering"
        );
    }

    #[test]
    fn bridge_events_match_the_captured_payloads() {
        // Byte for byte against what a running Zigbee2MQTT published for this
        // device, modulo key order.
        let mut store = StateStore::new(Topics::default());

        let joined = store.translate(&Event::DeviceJoined {
            ieee: DEVICE,
            nwk: rszigbee_spec::ids::Nwk::new(0x1111),
        });
        assert_eq!(joined[0].topic, "zigbee2mqtt/bridge/event");
        let value: Value = serde_json::from_str(&joined[0].payload).expect("valid json");
        assert_eq!(value["type"], "device_joined");
        assert_eq!(value["data"]["ieee_address"], "0xa4c138142d62ffff");
        assert_eq!(
            value["data"]["friendly_name"], "0xa4c138142d62ffff",
            "consumers read one or the other, so both are sent"
        );

        // The spelling that is easy to get wrong: `device_leave`, not
        // `device_left`, and it does not follow from the Rust variant name.
        let left = store.translate(&Event::DeviceLeft {
            ieee: DEVICE,
            reason: rszigbee_core::event::LeaveReason::Unknown,
        });
        let value: Value = serde_json::from_str(&left[0].payload).expect("valid json");
        assert_eq!(value["type"], "device_leave");
    }

    #[test]
    fn availability_is_retained_and_device_state_is_not() {
        let store = StateStore::new(Topics::default());
        assert_eq!(store.online().payload, r#"{"state":"online"}"#);
        assert_eq!(store.offline().payload, r#"{"state":"offline"}"#);
        assert!(
            store.online().retain,
            "a late subscriber must learn the gateway is up"
        );

        let mut store = store;
        let published = store.translate(&Event::StateChanged {
            ieee: DEVICE,
            endpoint: None,
            changes: StateChanges::new().with("battery", StateValue::Int(100)),
        });
        assert!(
            !published[0].retain,
            "a retained reading has no indication of its age, which for a \
             sensor is worse than no reading"
        );
    }

    #[test]
    fn a_permit_join_response_matches_the_captured_payload() {
        let store = StateStore::new(Topics::default());
        let publication = store.response("permit_join", &json!({"time": 254}), true);
        assert_eq!(publication.topic, "zigbee2mqtt/bridge/response/permit_join");
        assert_eq!(
            publication.payload,
            r#"{"data":{"time":254},"status":"ok"}"#
        );
    }

    #[test]
    fn an_event_with_no_place_in_the_contract_publishes_nothing() {
        // The control. A translation that emitted something for everything
        // would make the assertions above meaningless.
        let mut store = StateStore::new(Topics::default());
        assert!(
            store.translate(&Event::Lagged { skipped: 7 }).is_empty(),
            "an internal event must not reach the broker"
        );
    }
}
