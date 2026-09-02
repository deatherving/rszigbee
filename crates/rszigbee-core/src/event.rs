//! The event model.
//!
//! Four deliberate departures from `Zigbee2MQTT`'s internal model, all argued in
//! the README, "Events and commands":
//!
//! 1. [`Event::StateChanged`] carries **only the delta**. Publishing the whole
//!    cached state is an MQTT compatibility behaviour, not an application need;
//!    the MQTT layer re-applies it.
//! 2. [`Event::Action`] is **separate from state**. A button press is not
//!    state. Upstream has to fold actions into the state object and then
//!    exclude them again through `CACHE_IGNORE_PROPERTIES`; making the
//!    distinction structural removes that class of bug.
//! 3. **Raw and converted events coexist.** An unknown device still produces
//!    [`Event::ZclMessage`] and [`Event::UnparsedFrame`], so it stays useful
//!    with no definition at all.
//! 4. [`Event::ConverterFailed`] and [`Event::UnparsedFrame`] are **events, not
//!    log lines**. They are the answer to "why is my device not working", and
//!    they are countable.

use std::time::{Instant, SystemTime};

use rszigbee_adapter::{DisconnectReason, StartOutcome, TxFailure};
use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee, Nwk};
use rszigbee_spec::zcl::ZclValue;
use rszigbee_spec::zdo::ZdoClusterId;

use crate::capability::CapabilityId;
use crate::device::InterviewState;
use crate::reachability::{Evidence, Reachability};
use crate::state::StateChanges;

/// Why `last_seen` moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastSeenReason {
    /// A device announcement.
    Announce,
    /// A join.
    Joined,
    /// A frame that produced a state change.
    Message,
    /// A frame that produced nothing, but still proves the device is alive.
    MessageWithoutPayload,
    /// A network-address resolution.
    NetworkAddress,
}

/// Why a device left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveReason {
    /// The device asked to leave.
    DeviceRequest,
    /// We removed it.
    Removed,
    /// The coordinator reported it gone without a reason.
    Unknown,
}

/// One step of the interview, reported for diagnostics and progress display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterviewStep {
    /// Reading the node descriptor.
    NodeDescriptor,
    /// Enumerating endpoints.
    ActiveEndpoints,
    /// Reading one endpoint's simple descriptor.
    SimpleDescriptor(EndpointId),
    /// Reading `genBasic`.
    BasicAttributes,
    /// Enrolling IAS.
    IasEnroll(EndpointId),
    /// Binding and reading `genPollCtrl`.
    PollControl(EndpointId),
}

/// How a definition was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionSource {
    /// A shipped declarative definition.
    Bundled,
    /// A user-supplied definition from a directory.
    Local,
    /// Synthesised from the device's clusters.
    Generated,
}

/// Why a frame could not be turned into anything useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseFailure {
    /// The ZCL codec rejected it.
    Codec(rszigbee_spec::codec::CodecError),
    /// The cluster is not in the registry for this device.
    UnknownCluster(ClusterId),
    /// The command is not known for this cluster.
    UnknownCommand(u8),
    /// The attribute is not known, so its type could not be determined.
    UnknownAttribute(u16),
}

/// A decoded ZCL message, emitted for every frame whether or not a converter
/// handled it.
#[derive(Debug, Clone, PartialEq)]
pub struct ZclMessage {
    /// Sender.
    pub ieee: Ieee,
    /// Source endpoint.
    pub endpoint: EndpointId,
    /// Cluster.
    pub cluster: ClusterId,
    /// What kind of message.
    pub kind: ZclMessageKind,
    /// Link quality, when reported.
    pub link_quality: Option<u8>,
}

/// The shape of a decoded ZCL message.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ZclMessageKind {
    /// An attribute report or read response.
    Attributes(Vec<(u16, ZclValue)>),
    /// A cluster-specific command with its decoded parameters.
    Command {
        /// Command id.
        id: u8,
        /// Command name, when the registry knew it.
        name: Option<String>,
        /// Decoded parameters, in declaration order.
        params: Vec<(String, ZclValue)>,
    },
    /// A Default Response.
    DefaultResponse {
        /// The command being responded to.
        command: u8,
        /// The status byte.
        status: u8,
    },
}

/// Something happened.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Event {
    // ---- runtime lifecycle
    /// The runtime came up.
    Started {
        /// What starting the network did.
        outcome: StartOutcome,
    },
    /// Shutdown has begun.
    Stopping,
    /// The coordinator link went away.
    AdapterDisconnected {
        /// Why.
        reason: DisconnectReason,
    },
    /// The coordinator link came back.
    AdapterReconnected {
        /// What restarting the network did.
        outcome: StartOutcome,
    },
    /// This consumer fell behind and missed events.
    ///
    /// Surfaced rather than silently dropped: a slow consumer should be a
    /// visible bug, not a mystery gap in a timeline.
    Lagged {
        /// How many events were missed.
        skipped: u64,
    },

    // ---- network
    /// Permit-join state changed.
    PermitJoinChanged {
        /// Whether joining is currently allowed.
        permitted: bool,
        /// When it closes.
        until: Option<Instant>,
        /// The router joining is open through, if not the coordinator.
        via: Option<Ieee>,
    },

    // ---- device lifecycle
    /// A device joined.
    DeviceJoined {
        /// Address.
        ieee: Ieee,
        /// Short address.
        nwk: Nwk,
    },
    /// A device announced itself, usually after a power cycle.
    DeviceAnnounced {
        /// Address.
        ieee: Ieee,
    },
    /// A device left.
    DeviceLeft {
        /// Address.
        ieee: Ieee,
        /// Why.
        reason: LeaveReason,
    },
    /// A device's short address changed.
    DeviceAddressChanged {
        /// Address.
        ieee: Ieee,
        /// Previous short address.
        from: Nwk,
        /// New short address.
        to: Nwk,
    },
    /// An interview began.
    InterviewStarted {
        /// Address.
        ieee: Ieee,
    },
    /// An interview made progress.
    InterviewProgress {
        /// Address.
        ieee: Ieee,
        /// Which step.
        step: InterviewStep,
    },
    /// An interview finished.
    InterviewFinished {
        /// Address.
        ieee: Ieee,
        /// Final state.
        state: InterviewState,
    },
    /// A definition was matched, or was not.
    DefinitionResolved {
        /// Address.
        ieee: Ieee,
        /// The model that matched, or `None`.
        model: Option<String>,
        /// How it was arrived at.
        source: DefinitionSource,
    },
    /// A frame was received from a device, moving `last_seen`.
    LastSeenChanged {
        /// Address.
        ieee: Ieee,
        /// When.
        at: SystemTime,
        /// Why.
        reason: LastSeenReason,
    },
    /// Reachability changed.
    ReachabilityChanged {
        /// Address.
        ieee: Ieee,
        /// Previous belief.
        from: Reachability,
        /// New belief.
        to: Reachability,
        /// What changed it.
        evidence: Evidence,
    },

    // ---- capability layer
    /// Capability values changed. Delta only.
    StateChanged {
        /// Address.
        ieee: Ieee,
        /// Endpoint, when the device has more than one.
        endpoint: Option<EndpointId>,
        /// What changed.
        changes: StateChanges,
    },
    /// A momentary action occurred.
    Action {
        /// Address.
        ieee: Ieee,
        /// Endpoint, when the device has more than one.
        endpoint: Option<EndpointId>,
        /// Which capability emitted it.
        capability: CapabilityId,
        /// The action name.
        action: String,
    },

    // ---- raw layer
    /// A decoded ZCL message. Emitted for every frame, including from devices
    /// with no definition.
    ZclMessage(ZclMessage),
    /// A ZDO response arrived.
    ZdoResponse {
        /// Sender.
        nwk: Nwk,
        /// Cluster.
        cluster: ZdoClusterId,
        /// Raw payload.
        payload: Vec<u8>,
    },
    /// A converter raised an error. An event rather than a log line, because
    /// this is what a user needs to see to understand a misbehaving device.
    ConverterFailed {
        /// Address.
        ieee: Ieee,
        /// Cluster involved.
        cluster: ClusterId,
        /// What went wrong.
        detail: String,
    },
    /// A frame could not be decoded at all.
    UnparsedFrame {
        /// Address.
        ieee: Ieee,
        /// Endpoint.
        endpoint: EndpointId,
        /// Cluster.
        cluster: ClusterId,
        /// The bytes, kept so a user can report them.
        raw: Vec<u8>,
        /// Why decoding failed.
        reason: ParseFailure,
    },
    /// A command to a device failed.
    CommandFailed {
        /// Address.
        ieee: Ieee,
        /// Which capability was being written.
        capability: Option<CapabilityId>,
        /// Why.
        failure: TxFailure,
    },
}

impl Event {
    /// The device this event concerns, when it concerns one.
    ///
    /// Exists so a consumer can filter by device without matching every
    /// variant — and so adding a variant does not break that filtering.
    #[must_use]
    pub const fn ieee(&self) -> Option<Ieee> {
        match self {
            Self::DeviceJoined { ieee, .. }
            | Self::DeviceAnnounced { ieee }
            | Self::DeviceLeft { ieee, .. }
            | Self::DeviceAddressChanged { ieee, .. }
            | Self::InterviewStarted { ieee }
            | Self::InterviewProgress { ieee, .. }
            | Self::InterviewFinished { ieee, .. }
            | Self::DefinitionResolved { ieee, .. }
            | Self::LastSeenChanged { ieee, .. }
            | Self::ReachabilityChanged { ieee, .. }
            | Self::StateChanged { ieee, .. }
            | Self::Action { ieee, .. }
            | Self::ConverterFailed { ieee, .. }
            | Self::UnparsedFrame { ieee, .. }
            | Self::CommandFailed { ieee, .. } => Some(*ieee),
            Self::ZclMessage(m) => Some(m.ieee),
            _ => None,
        }
    }

    /// True for events that indicate something is wrong with a device, as
    /// opposed to normal operation. These are the ones worth surfacing in a
    /// diagnostic view and counting as a metric.
    #[must_use]
    pub const fn is_diagnostic(&self) -> bool {
        matches!(
            self,
            Self::ConverterFailed { .. }
                | Self::UnparsedFrame { .. }
                | Self::CommandFailed { .. }
                | Self::Lagged { .. }
                | Self::AdapterDisconnected { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateValue;

    fn ieee() -> Ieee {
        Ieee::new(0x0017_8801_00dc_4d3f)
    }

    #[test]
    fn device_events_expose_their_address_uniformly() {
        let cases = vec![
            Event::DeviceJoined {
                ieee: ieee(),
                nwk: Nwk::new(1),
            },
            Event::DeviceAnnounced { ieee: ieee() },
            Event::InterviewStarted { ieee: ieee() },
            Event::StateChanged {
                ieee: ieee(),
                endpoint: None,
                changes: StateChanges::new().with("state", StateValue::Bool(true)),
            },
            Event::Action {
                ieee: ieee(),
                endpoint: None,
                capability: "action".into(),
                action: "single".into(),
            },
        ];
        for e in cases {
            assert_eq!(e.ieee(), Some(ieee()), "{e:?}");
        }
    }

    #[test]
    fn runtime_events_have_no_device() {
        assert_eq!(Event::Stopping.ieee(), None);
        assert_eq!(Event::Lagged { skipped: 3 }.ieee(), None);
        assert_eq!(
            Event::Started {
                outcome: StartOutcome::Resumed
            }
            .ieee(),
            None
        );
    }

    #[test]
    fn state_and_action_are_separate_events() {
        // The whole point of the departure from upstream: a button press must
        // not have to become a state field that then needs excluding again.
        let state = Event::StateChanged {
            ieee: ieee(),
            endpoint: None,
            changes: StateChanges::new().with("brightness", StateValue::Int(1)),
        };
        let action = Event::Action {
            ieee: ieee(),
            endpoint: None,
            capability: "action".into(),
            action: "hold".into(),
        };
        assert_ne!(
            core::mem::discriminant(&state),
            core::mem::discriminant(&action)
        );
    }

    #[test]
    fn state_changes_carry_a_delta_not_a_full_snapshot() {
        // If this ever becomes a snapshot, the MQTT compatibility behaviour has
        // leaked into the core event model.
        let e = Event::StateChanged {
            ieee: ieee(),
            endpoint: None,
            changes: StateChanges::new().with("temperature", StateValue::Float(21.5)),
        };
        if let Event::StateChanged { changes, .. } = &e {
            assert_eq!(changes.len(), 1);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn failures_are_classified_as_diagnostic() {
        assert!(
            Event::ConverterFailed {
                ieee: ieee(),
                cluster: ClusterId(0x0006),
                detail: "no converter".into()
            }
            .is_diagnostic()
        );
        assert!(
            Event::UnparsedFrame {
                ieee: ieee(),
                endpoint: EndpointId(1),
                cluster: ClusterId(0xfc03),
                raw: vec![0xff],
                reason: ParseFailure::UnknownCluster(ClusterId(0xfc03)),
            }
            .is_diagnostic()
        );
        assert!(Event::Lagged { skipped: 1 }.is_diagnostic());
        // Normal traffic is not diagnostic.
        assert!(!Event::DeviceAnnounced { ieee: ieee() }.is_diagnostic());
    }

    #[test]
    fn an_unknown_device_still_produces_a_usable_event() {
        // A device with no definition must not be silent, or a user has nothing
        // to contribute a definition from.
        let e = Event::ZclMessage(ZclMessage {
            ieee: ieee(),
            endpoint: EndpointId(1),
            cluster: ClusterId(0xef00),
            kind: ZclMessageKind::Attributes(vec![(0x0000, ZclValue::Uint(1))]),
            link_quality: Some(90),
        });
        assert_eq!(e.ieee(), Some(ieee()));
        assert!(!e.is_diagnostic());
    }
}
