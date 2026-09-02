//! Adapter error types.
//!
//! Errors are typed per layer with `source()` chains, so a caller can decide
//! whether to retry, re-route, mark a device unreachable, or give up — which is
//! impossible with a stringly-typed error.

use core::time::Duration;

use rszigbee_spec::codec::CodecError;

/// Why a transmit failed.
///
/// The distinction between these variants drives real behaviour: `NoAck` on a
/// sleepy device is normal and means "queue it", while `NoAck` on a mains router
/// is evidence of unreachability. Collapsing them loses that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TxFailure {
    /// No APS acknowledgement within the deadline.
    #[error("no acknowledgement from the device")]
    NoAck,
    /// The coordinator could not find a route.
    #[error("no route to the device")]
    NoRoute,
    /// The coordinator's own buffers or tables are full. Back off and retry;
    /// this says nothing about the device.
    #[error("coordinator is out of resources")]
    CoordinatorBusy,
    /// The device rejected the frame at the APS layer.
    #[error("the device rejected the frame")]
    Rejected,
    /// The deadline expired with no outcome at all.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// The adapter reported an error this build does not model.
    #[error("adapter-specific failure (status {0:#04x})")]
    AdapterStatus(u16),
}

impl TxFailure {
    /// True when retrying the same request could plausibly succeed without
    /// anything else changing.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::CoordinatorBusy | Self::NoAck | Self::Timeout(_))
    }
}

/// Why the adapter link went away.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// The serial port disappeared — unplugged, or re-enumerated by the OS.
    #[error("serial port closed")]
    SerialClosed,
    /// The coordinator reset itself; watchdog, brown-out or firmware crash.
    #[error("coordinator reset unexpectedly")]
    CoordinatorReset,
    /// The transport desynchronised beyond recovery.
    #[error("protocol desynchronised: {0}")]
    ProtocolError(String),
    /// A local `stop()` call.
    #[error("stopped by request")]
    Requested,
}

/// An adapter-level error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// The transport is not open.
    #[error("adapter is not connected")]
    NotConnected,
    /// The link went away mid-operation.
    #[error("adapter disconnected: {0}")]
    Disconnected(#[from] DisconnectReason),
    /// A transmit failed.
    #[error("transmit failed: {0}")]
    Tx(#[from] TxFailure),
    /// A frame could not be encoded or decoded.
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),
    /// The operation is not supported by this adapter.
    ///
    /// A first-class variant rather than a panic, because the trait
    /// deliberately includes methods that only some coordinators implement
    /// (`InterPAN`, backup) and the runtime must be able to ask and be told no.
    #[error("{0} is not supported by this adapter")]
    Unsupported(&'static str),
    /// The coordinator's firmware is too old or too new.
    #[error("incompatible coordinator firmware: {0}")]
    IncompatibleFirmware(String),
    /// Startup found a network that does not match the configuration. Never
    /// resolved by guessing: forming a new network here would orphan every
    /// device the user owns.
    #[error("coordinator network does not match configuration: {0}")]
    NetworkMismatch(String),
    /// The transport failed at the OS level.
    #[error("transport error: {0}")]
    Transport(String),
    /// A deadline expired.
    #[error("operation timed out after {0:?}")]
    Timeout(Duration),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_failures_are_the_ones_worth_retrying() {
        assert!(TxFailure::CoordinatorBusy.is_transient());
        assert!(TxFailure::NoAck.is_transient());
        assert!(TxFailure::Timeout(Duration::from_secs(1)).is_transient());
        // A rejected frame will be rejected again; retrying just makes noise.
        assert!(!TxFailure::Rejected.is_transient());
        assert!(!TxFailure::NoRoute.is_transient());
    }

    #[test]
    fn errors_carry_their_source_for_diagnostics() {
        let e = AdapterError::from(TxFailure::NoRoute);
        assert!(std::error::Error::source(&e).is_some());
        assert_eq!(e.to_string(), "transmit failed: no route to the device");
    }

    #[test]
    fn unsupported_names_the_operation_so_the_message_is_actionable() {
        let e = AdapterError::Unsupported("coordinator backup");
        assert_eq!(
            e.to_string(),
            "coordinator backup is not supported by this adapter"
        );
    }
}
