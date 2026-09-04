//! Topic construction and parsing.

use rszigbee_spec::ids::Ieee;

/// The topic namespace a gateway publishes under.
///
/// `zigbee2mqtt` by default, because that is what every existing consumer is
/// configured for. Changing it is supported; changing it silently would break
/// every subscriber, so it is a constructor argument rather than a setting
/// applied later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topics {
    base: String,
}

impl Default for Topics {
    fn default() -> Self {
        Self::new("zigbee2mqtt")
    }
}

impl Topics {
    /// A namespace rooted at `base`.
    ///
    /// A trailing slash is trimmed. Left in, every topic would contain a `//`,
    /// which MQTT treats as an empty level rather than as a typo — so
    /// `zigbee2mqtt//bridge/state` is a *different* topic from
    /// `zigbee2mqtt/bridge/state` and every subscriber misses it.
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// The namespace root.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Where a device's state is published.
    ///
    /// Keyed by IEEE address. `Zigbee2MQTT` uses a friendly name when one is
    /// configured and the address otherwise; the address is what it falls back
    /// to and what this always uses, because it is the only identifier that is
    /// stable and needs no configuration.
    #[must_use]
    pub fn device(&self, ieee: Ieee) -> String {
        format!("{}/{ieee}", self.base)
    }

    /// Where a caller writes desired state for a device.
    #[must_use]
    pub fn device_set(&self, ieee: Ieee) -> String {
        format!("{}/{ieee}/set", self.base)
    }

    /// Where a caller asks a device to report current state.
    #[must_use]
    pub fn device_get(&self, ieee: Ieee) -> String {
        format!("{}/{ieee}/get", self.base)
    }

    /// Whether the gateway is up.
    #[must_use]
    pub fn bridge_state(&self) -> String {
        format!("{}/bridge/state", self.base)
    }

    /// Things that happened: a device joined, left, or was interviewed.
    #[must_use]
    pub fn bridge_event(&self) -> String {
        format!("{}/bridge/event", self.base)
    }

    /// A request to the gateway, e.g. `permit_join`.
    #[must_use]
    pub fn bridge_request(&self, name: &str) -> String {
        format!("{}/bridge/request/{name}", self.base)
    }

    /// The answer to a `bridge/request`.
    #[must_use]
    pub fn bridge_response(&self, name: &str) -> String {
        format!("{}/bridge/response/{name}", self.base)
    }

    /// Everything a gateway must subscribe to, as MQTT filters.
    ///
    /// Returned rather than left to the caller so a new inbound topic cannot be
    /// handled but never subscribed to — which fails as silence, the hardest
    /// thing to notice.
    #[must_use]
    pub fn subscriptions(&self) -> Vec<String> {
        vec![
            format!("{}/+/set", self.base),
            format!("{}/+/get", self.base),
            format!("{}/bridge/request/+", self.base),
        ]
    }

    /// Splits a topic into the part after the base, or `None` if it is not ours.
    ///
    /// A broker can deliver a topic outside our namespace — a shared
    /// subscription, an overlapping wildcard, a misconfigured bridge — and
    /// treating one as ours would act on another system's messages.
    #[must_use]
    pub fn strip<'a>(&self, topic: &'a str) -> Option<&'a str> {
        let rest = topic.strip_prefix(&self.base)?;
        // The prefix must end at a level boundary: `zigbee2mqtt-test/x/set`
        // starts with `zigbee2mqtt` as a *string* and is a different namespace.
        rest.strip_prefix('/')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: Ieee = Ieee::new(0xa4c1_3814_2d62_ffff);

    #[test]
    fn topics_match_the_captured_ones() {
        // Compared against topics captured from a running Zigbee2MQTT rather
        // than to a restatement of the code: these exact strings appeared in
        // its log while it drove the valve this crate was written against.
        let t = Topics::default();
        assert_eq!(t.device(DEVICE), "zigbee2mqtt/0xa4c138142d62ffff");
        assert_eq!(t.device_set(DEVICE), "zigbee2mqtt/0xa4c138142d62ffff/set");
        assert_eq!(t.bridge_state(), "zigbee2mqtt/bridge/state");
        assert_eq!(t.bridge_event(), "zigbee2mqtt/bridge/event");
        assert_eq!(
            t.bridge_response("permit_join"),
            "zigbee2mqtt/bridge/response/permit_join"
        );
        assert_eq!(
            t.bridge_request("permit_join"),
            "zigbee2mqtt/bridge/request/permit_join"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_produce_an_empty_topic_level() {
        // MQTT treats `//` as an empty level, not as a typo, so the topic would
        // be silently different from the one every subscriber expects.
        assert_eq!(
            Topics::new("zigbee2mqtt/").bridge_state(),
            "zigbee2mqtt/bridge/state"
        );
    }

    #[test]
    fn a_topic_from_another_namespace_is_not_ours() {
        let t = Topics::default();
        assert_eq!(t.strip("zigbee2mqtt/0x1/set"), Some("0x1/set"));
        // The negative control, and a real hazard: this starts with our base as
        // a string but is a different namespace.
        assert_eq!(t.strip("zigbee2mqtt-test/0x1/set"), None);
        assert_eq!(t.strip("othergateway/0x1/set"), None);
    }

    #[test]
    fn every_inbound_topic_is_subscribed() {
        // The pairing that fails as silence: a request the gateway handles but
        // never subscribed to simply never arrives.
        let t = Topics::default();
        let subs = t.subscriptions();
        let matches = |topic: &str| {
            subs.iter().any(|filter| {
                let f: Vec<&str> = filter.split('/').collect();
                let s: Vec<&str> = topic.split('/').collect();
                f.len() == s.len() && f.iter().zip(&s).all(|(fl, sl)| *fl == "+" || fl == sl)
            })
        };
        assert!(matches(&t.device_set(DEVICE)), "set must be subscribed");
        assert!(matches(&t.device_get(DEVICE)), "get must be subscribed");
        assert!(
            matches(&t.bridge_request("permit_join")),
            "bridge requests must be subscribed"
        );
        // And not the outbound ones: subscribing to our own publishes would
        // feed the gateway its own state as a command.
        assert!(!matches(&t.device(DEVICE)), "state is published, not read");
        assert!(!matches(&t.bridge_state()), "bridge state is published");
        assert!(
            !matches(&t.bridge_response("permit_join")),
            "our own responses must not come back as requests"
        );
    }
}
