//! The rszigbee runtime model.
//!
//! This crate holds the typed device, event, command, capability, state,
//! reachability and persistence model that both operating modes share. Phase 1
//! establishes the types and the boundaries; the runtime task that drives them
//! lands with the Phase 2 vertical slice.
//!
//! # Boundaries this crate enforces
//!
//! * **No MQTT.** No dependency on `rszigbee-mqtt`, on an MQTT client, or on
//!   JSON for internal paths. CI asserts it.
//! * **No Home Assistant.** Not a concept that exists here.
//! * **State deltas, not snapshots.** [`Event::StateChanged`] carries what
//!   changed; publishing a merged snapshot is a compatibility behaviour that
//!   belongs to the MQTT layer.
//! * **Actions are not state.** See [`event`].
//! * **Facts here, policy injected.** See [`reachability`].
//!
//! # Layout
//!
//! | module | what |
//! |---|---|
//! | [`device`] | the device and endpoint model, interview state |
//! | [`capability`] | what a device can report or be told |
//! | [`state`] | capability values and ordered deltas |
//! | [`event`] | everything the runtime reports |
//! | [`command`] | everything the runtime accepts |
//! | [`reachability`] | reachability facts and the policy seam |
//! | [`store`] | persistence traits and an in-memory backend |

#![forbid(unsafe_code)]

pub mod capability;
pub mod command;
pub mod device;
pub mod event;
pub mod reachability;
pub mod state;
pub mod store;

pub use capability::{
    Access, Capability, CapabilityId, CapabilityKind, Category, Unit, ValueDomain,
};
pub use command::{
    Brightness, Color, CommandError, CommandOutcome, Confirmation, DeviceCommand, Mireds, Percent,
};
pub use device::{BasicInfo, DeviceInfo, DeviceKind, EndpointInfo, InterviewState, PowerSource};
pub use event::{Event, InterviewStep, LastSeenReason, LeaveReason, ZclMessage, ZclMessageKind};
pub use reachability::{
    Assessment, NextCheck, ProbeResult, Reachability, ReachabilityContext, ReachabilityInfo,
    ReachabilityPolicy, SilencePolicy,
};
pub use state::{Priority, StateChanges, StateSnapshot, StateValue};
pub use store::{MemoryStore, PersistedDevice, PersistedNetwork, StoreError, ZigbeeStore};

/// Re-exported so downstream crates need not depend on the adapter crate
/// directly for the handful of types that cross into the public API.
pub mod adapter {
    pub use rszigbee_adapter::{
        AdapterCapabilities, AdapterError, AdapterEvent, CoordinatorAdapter, DisconnectReason,
        MockAdapter, MockHandle, NetworkConfig, StartOutcome, TxFailure,
    };
}
