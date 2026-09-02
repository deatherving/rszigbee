//! Transmit and receive types crossing the adapter boundary.
//!
//! These replace zigbee-herdsman's positional-argument methods
//! (`sendZclFrameToEndpoint` takes nine positional parameters). A struct with
//! named fields and a builder is not a style preference here: the ninth
//! argument of a nine-argument call is where `disableRecovery` gets confused
//! with `disableResponse`, and that mistake is invisible at the call site.

use core::time::Duration;

use rszigbee_spec::ids::{ClusterId, EndpointId, GroupId, Ieee, Nwk, ProfileId};
use rszigbee_spec::zdo::ZdoClusterId;

use crate::error::TxFailure;

/// Where a frame is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// A single device. Both addresses are carried because adapters differ in
    /// which one they want, and resolving one from the other is the runtime's
    /// job, not each adapter's.
    Unicast {
        /// Permanent address.
        ieee: Ieee,
        /// Current short address.
        nwk: Nwk,
    },
    /// A Zigbee group.
    Group(GroupId),
    /// A broadcast address.
    Broadcast(BroadcastAddress),
}

/// The broadcast addresses Zigbee defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastAddress {
    /// `0xffff` — every device.
    All,
    /// `0xfffd` — devices with receiver on when idle.
    RxOnWhenIdle,
    /// `0xfffc` — routers and the coordinator.
    Routers,
    /// `0xfff8` — low-power routers.
    LowPowerRouters,
}

impl BroadcastAddress {
    /// The wire value.
    #[must_use]
    pub const fn to_nwk(self) -> Nwk {
        Nwk::new(match self {
            Self::All => 0xffff,
            Self::RxOnWhenIdle => 0xfffd,
            Self::Routers => 0xfffc,
            Self::LowPowerRouters => 0xfff8,
        })
    }
}

/// How the runtime wants a request handled when the device is not immediately
/// reachable.
///
/// The vocabulary is upstream's (`SendPolicy` in zigbee-herdsman's
/// `controller/tstype.ts`) because the semantics are load-bearing for sleepy
/// devices and were arrived at through years of field experience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendPolicy {
    /// Send as soon as possible; retry per the adapter's own rules. Default.
    #[default]
    Queue,
    /// Send now and do not keep it for later. Used for responses and for the
    /// reads during an interview, where a stale retry is worse than a failure.
    Immediate,
    /// Must be sent in order with other bulk requests.
    Bulk,
    /// If delivery fails, keep only one copy of this exact payload.
    KeepPayload,
    /// If delivery fails, keep only the newest request per command id.
    KeepCommand,
}

/// Per-request options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxOptions {
    /// Wait for a response before completing.
    pub expect_response: bool,
    /// Ask the peer not to send a Default Response.
    pub disable_default_response: bool,
    /// Suppress adapter-level route repair. Set when a failure is expected and
    /// cheap (an availability probe), because repair is slow and noisy.
    pub disable_recovery: bool,
    /// Overall deadline.
    pub timeout: Duration,
    /// Queueing behaviour.
    pub policy: SendPolicy,
}

impl Default for TxOptions {
    fn default() -> Self {
        Self {
            expect_response: true,
            disable_default_response: false,
            disable_recovery: false,
            // Upstream's default; per-definition `meta.timeout` overrides it.
            timeout: Duration::from_secs(10),
            policy: SendPolicy::Queue,
        }
    }
}

impl TxOptions {
    /// Fire and forget.
    #[must_use]
    pub fn no_response() -> Self {
        Self {
            expect_response: false,
            ..Self::default()
        }
    }

    /// For probes: fail fast, do not repair routes.
    #[must_use]
    pub fn probe(timeout: Duration) -> Self {
        Self {
            expect_response: true,
            disable_default_response: true,
            disable_recovery: true,
            timeout,
            policy: SendPolicy::Immediate,
        }
    }

    /// Sets the send policy.
    #[must_use]
    pub const fn with_policy(mut self, policy: SendPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// A ZCL transmit request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZclTx {
    /// Where it goes.
    pub dest: Destination,
    /// Destination endpoint. Ignored for group and broadcast destinations.
    pub endpoint: EndpointId,
    /// Source endpoint on the coordinator.
    pub source_endpoint: EndpointId,
    /// Application profile.
    pub profile: ProfileId,
    /// The cluster.
    pub cluster: ClusterId,
    /// A complete, already-encoded ZCL frame including its header.
    ///
    /// Encoding happens above the adapter so that exactly one implementation of
    /// the ZCL codec exists, and so an adapter cannot silently reinterpret a
    /// frame. Adapters that need the header fields parse them back.
    pub frame: Vec<u8>,
    /// Options.
    pub options: TxOptions,
}

impl ZclTx {
    /// A unicast request with default options.
    #[must_use]
    pub fn unicast(
        ieee: Ieee,
        nwk: Nwk,
        endpoint: EndpointId,
        cluster: ClusterId,
        frame: Vec<u8>,
    ) -> Self {
        Self {
            dest: Destination::Unicast { ieee, nwk },
            endpoint,
            source_endpoint: EndpointId::HA,
            profile: ProfileId::HA,
            cluster,
            frame,
            options: TxOptions::default(),
        }
    }

    /// Replaces the options.
    #[must_use]
    pub fn with_options(mut self, options: TxOptions) -> Self {
        self.options = options;
        self
    }
}

/// A received ZCL frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZclRx {
    /// The sender, when known. A frame can arrive from a device the runtime has
    /// no record of, which is why this is optional rather than invented.
    pub ieee: Option<Ieee>,
    /// The sender's short address.
    pub nwk: Nwk,
    /// Source endpoint.
    pub endpoint: EndpointId,
    /// Destination endpoint on the coordinator.
    pub destination_endpoint: EndpointId,
    /// The cluster.
    pub cluster: ClusterId,
    /// The group, when the frame was addressed to one.
    pub group: Option<GroupId>,
    /// True when the frame arrived as a broadcast.
    pub was_broadcast: bool,
    /// Link quality, `0..=255`, when the adapter reports it.
    pub link_quality: Option<u8>,
    /// The complete ZCL frame including its header.
    pub frame: Vec<u8>,
}

/// A ZDO transmit request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZdoTx {
    /// Where it goes.
    pub dest: Destination,
    /// The ZDO cluster.
    pub cluster: ZdoClusterId,
    /// The already-encoded ZDO payload, without the transaction sequence number
    /// (adapters differ on whether they prepend it themselves — this is what
    /// upstream's `hasZdoMessageOverhead` flag is about).
    pub payload: Vec<u8>,
    /// Options.
    pub options: TxOptions,
}

/// The outcome of a transmit that expected no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxConfirm {
    /// The peer acknowledged at the APS layer.
    Acked,
    /// Sent; no acknowledgement was requested.
    Sent,
    /// Buffered for a sleepy device; it will go out on the next check-in.
    Queued,
    /// Delivery failed.
    Failed(TxFailure),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_addresses_map_to_the_right_wire_values() {
        assert_eq!(BroadcastAddress::All.to_nwk().raw(), 0xffff);
        assert_eq!(BroadcastAddress::RxOnWhenIdle.to_nwk().raw(), 0xfffd);
        assert_eq!(BroadcastAddress::Routers.to_nwk().raw(), 0xfffc);
        // Every broadcast address must be recognised as one.
        for b in [
            BroadcastAddress::All,
            BroadcastAddress::RxOnWhenIdle,
            BroadcastAddress::Routers,
            BroadcastAddress::LowPowerRouters,
        ] {
            assert!(b.to_nwk().is_broadcast(), "{b:?}");
        }
    }

    #[test]
    fn default_options_match_upstream_behaviour() {
        let d = TxOptions::default();
        assert!(d.expect_response);
        assert!(!d.disable_recovery);
        assert_eq!(d.timeout, Duration::from_secs(10));
        assert_eq!(d.policy, SendPolicy::Queue);
    }

    #[test]
    fn probe_options_fail_fast_and_do_not_repair_routes() {
        // An availability probe that triggers route repair turns a cheap check
        // into an expensive one and distorts the very thing it measures.
        let p = TxOptions::probe(Duration::from_secs(2));
        assert!(p.disable_recovery);
        assert_eq!(p.policy, SendPolicy::Immediate);
        assert_eq!(p.timeout, Duration::from_secs(2));
    }

    #[test]
    fn unicast_defaults_to_the_home_automation_profile_and_endpoint() {
        let tx = ZclTx::unicast(
            Ieee::new(1),
            Nwk::new(2),
            EndpointId(1),
            ClusterId(0x0006),
            vec![0x01, 0x00, 0x01],
        );
        assert_eq!(tx.profile, ProfileId::HA);
        assert_eq!(tx.source_endpoint, EndpointId::HA);
        assert!(tx.options.expect_response);
    }
}
