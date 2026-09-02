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
//! | [`core`] | `adapter`, `spec` | MQTT, JSON, Home Assistant, **EZSP** |
//!
//! The last row is the point. `rszigbee-core` does not know EZSP exists; only
//! the Ember adapter does, and the [`CoordinatorAdapter`] trait is the seam.
//! `scripts/check-boundaries.sh` fails the build if that stops being true.
//!
//! # Status
//!
//! Phase 1 and the start of Phase 2. The types and the Ember adapter's
//! lifecycle are in place; the runtime that drives them is not, so there is no
//! `Zigbee::builder()` yet. What works today is the adapter directly:
//!
//! ```no_run
//! use rszigbee::adapter::{CoordinatorAdapter, MismatchPolicy, NetworkConfig};
//! use rszigbee::ember::EmberAdapter;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Serial settings come from a fingerprint table, because guessing hardware
//! // flow control wrong is a kernel-level hang rather than an error.
//! let (mut adapter, mut events) = EmberAdapter::serial("/dev/ttyUSB0").build();
//!
//! let outcome = adapter
//!     .start(
//!         &NetworkConfig {
//!             pan_id: None,
//!             extended_pan_id: None,
//!             channel: 11,
//!             network_key: None,
//!             // Refuse to form a network we did not mean to form.
//!             on_mismatch: MismatchPolicy::Fail,
//!         },
//!         None,
//!     )
//!     .await?;
//! println!("coordinator {} ({outcome:?})", adapter.coordinator_ieee().await?);
//!
//! while let Some(event) = events.recv().await {
//!     println!("{event:?}");
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
pub use rszigbee_core::{
    Brightness, Capability, CapabilityId, CommandError, DeviceCommand, DeviceInfo, Event,
    Reachability, StateChanges, StateValue, ZigbeeStore,
};
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
