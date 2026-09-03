//! A Rust-native Zigbee stack.
//!
//! This is the crate to depend on:
//!
//! ```toml
//! [dependencies]
//! rszigbee = { version = "0.0", features = ["ember"] }
//! ```
//!
//! Everything it re-exports lives in smaller crates underneath. Those exist to
//! enforce dependency boundaries, not to be composed by hand — depending on
//! them directly works but buys nothing.
//!
//! # The boundaries this facade hides
//!
//! | crate | may depend on | must never |
//! |---|---|---|
//! | [`spec`] | nothing but codecs | tokio, serial, I/O of any kind |
//! | [`adapter`] | `spec` | a concrete coordinator protocol |
//! | `rszigbee-adapter-ember` | `ezsp`, `ashv2`, `tokio-serial` | — |
//! | [`devices`] | `spec` | `rszigbee-core`, so definitions stay data |
//! | [`core`] | `adapter`, `spec`, `devices` | MQTT, JSON, Home Assistant, **EZSP** |
//!
//! The last row is the point. `rszigbee-core` does not know EZSP exists; only
//! the Ember adapter does, and the [`CoordinatorAdapter`] trait is the seam.
//! `scripts/check-boundaries.sh` fails the build if that stops being true.
//!
//! # The four extension points
//!
//! Everything a caller can replace or add to goes through one of these, and
//! they are all exported at the crate root rather than buried at different
//! depths:
//!
//! | trait | replaces | bounds |
//! |---|---|---|
//! | [`CoordinatorAdapter`] | the radio | `Send + 'static` |
//! | [`ZigbeeStore`] | persistence | `Send + Sync + 'static` |
//! | [`ReachabilityPolicy`] | when to consider a device gone | `Send + Sync + 'static` |
//! | [`DeviceBehavior`] | behaviour a definition cannot express | `Send + Sync + 'static` |
//!
//! The adapter is the odd one out, and deliberately: it is one serial port with
//! one framing state machine, so it is owned exclusively by the runtime task
//! and its methods take `&mut self`. Not being `Sync` makes concurrent use a
//! compile error rather than a rule in a comment.
//!
//! # Status
//!
//! Early. The runtime, the codecs, persistence and the Ember adapter's
//! lifecycle work; the device-compatibility engine and the MQTT layer do not
//! exist yet, so capability-level commands (`SetOn` and friends) are refused
//! rather than guessed. The ZCL escape hatch works today.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use rszigbee::ember::EmberAdapter;
//! use rszigbee::{Event, FileStore, Zigbee};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Serial settings come from a fingerprint table, because guessing hardware
//! // flow control wrong is a kernel-level hang rather than an error.
//! let (adapter, adapter_events) = EmberAdapter::serial("/dev/ttyUSB0").build();
//! let store = FileStore::open("./rszigbee-data").await?;
//!
//! // The default refuses to form a network: forming one when we should have
//! // resumed orphans every joined device.
//! let zigbee = Zigbee::builder(adapter, adapter_events, store).start().await?;
//! println!("coordinator {} ({:?})", zigbee.coordinator(), zigbee.start_outcome());
//!
//! zigbee.permit_join(Duration::from_secs(60), None).await?;
//!
//! let mut events = zigbee.events();
//! while let Some(event) = events.recv().await {
//!     match event {
//!         Event::DeviceJoined { ieee, .. } => println!("joined: {ieee}"),
//!         Event::InterviewFinished { ieee, state } => {
//!             println!("{ieee} interviewed: {state:?}");
//!         }
//!         Event::ZclMessage(message) => println!("{message:?}"),
//!         other => println!("{other:?}"),
//!     }
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

/// Specification-derived types, data and codecs: ZCL, ZDO, data types,
/// manufacturer codes. Sans-IO.
pub mod spec {
    pub use rszigbee_spec::*;
}

/// The coordinator adapter boundary, shared transport types, and a mock
/// adapter for testing without hardware.
pub mod adapter {
    pub use rszigbee_adapter::*;
}

/// The runtime model: devices, capabilities, state, events, commands,
/// reachability, persistence.
pub mod core {
    pub use rszigbee_core::*;
}

/// Device definitions and the matcher that resolves one for a device.
///
/// Resolution is verified to agree with `zigbee-herdsman-converters` across its
/// whole catalogue; see that crate's differential test.
pub mod devices {
    pub use rszigbee_devices::*;
}

/// Silicon Labs `EmberZNet` coordinators (EZSP over `ASHv2`).
///
/// This is the only module in the public API that knows EZSP exists.
#[cfg(feature = "ember")]
#[cfg_attr(docsrs, doc(cfg(feature = "ember")))]
pub mod ember {
    pub use rszigbee_adapter_ember::*;
}

// The types an application touches constantly, at the crate root so the common
// case needs no module paths.
pub use rszigbee_adapter::{
    AdapterCapabilities, AdapterError, AdapterEvent, CoordinatorAdapter, MismatchPolicy,
    NetworkConfig, StartOutcome,
};
#[cfg(feature = "file-store")]
#[cfg_attr(docsrs, doc(cfg(feature = "file-store")))]
pub use rszigbee_core::FileStore;
pub use rszigbee_core::{
    Brightness, Capability, CapabilityId, CommandError, DeviceCommand, DeviceInfo, Event,
    EventStream, InterviewOutcome, MemoryStore, PersistedDevice, PersistedNetwork, Reachability,
    RuntimeError, StateChanges, StateValue, StoreError, Zigbee, ZigbeeBuilder, ZigbeeStore,
};
// The four extension points, together and equally reachable. Two of them used
// to be a module path deeper than the others for no reason a caller could see,
// which made the escape hatch and the availability policy look like internals
// rather than the seams they are.
pub use rszigbee_core::{
    ConfigureContext, DecodeContext, DeviceBehavior, EncodeContext, Outcome, ReachabilityPolicy,
};
pub use rszigbee_devices::{Definition, DefinitionIndex, DeviceMatch};
pub use rszigbee_spec::ids::{ClusterId, EndpointId, GroupId, Ieee, Nwk};

#[cfg(test)]
mod tests {
    #[test]
    fn the_common_types_are_reachable_without_module_paths() {
        // The facade's job: an application should not need to know which
        // internal crate a type came from.
        let _ = super::Ieee::new(0x0017_8801_00dc_4d3f);
        let _ = super::MismatchPolicy::default();
        let _ = super::StateValue::Bool(true);
        let _ = super::Brightness::from_percent(50);
    }

    #[test]
    fn the_mock_adapter_is_available_without_a_hardware_feature() {
        // Testing an application against rszigbee must not require a dongle or
        // a feature flag.
        let (_a, _h, _rx) = super::adapter::MockAdapter::new();
    }

    #[cfg(feature = "ember")]
    #[test]
    fn the_ember_adapter_is_behind_its_feature_and_reachable_through_it() {
        let (a, _rx) = super::ember::EmberAdapter::serial("/dev/ttyUSB0").build();
        assert!(!super::CoordinatorAdapter::capabilities(&a).backup);
    }
}
