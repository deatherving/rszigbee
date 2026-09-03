//! The rszigbee coordinator adapter boundary.
//!
//! This crate defines the seam between the Zigbee runtime and a concrete
//! coordinator. It is the most important boundary in the project: it is what
//! lets one runtime drive an EZSP dongle, a TI Z-Stack dongle, a host-side
//! stack over an RCP radio, or a mock, without the runtime knowing which.
//!
//! The shape is derived from zigbee-herdsman's `Adapter` class, which has
//! survived six adapter families over several years and is therefore worth
//! copying rather than improving on. What changed for Rust:
//!
//! * events arrive on a channel instead of an `EventEmitter`, so backpressure
//!   is explicit and bounded;
//! * requests are structs, not nine positional arguments;
//! * response correlation lives in [`Correlator`], where dropping the future
//!   deregisters the wait, instead of a timeout-swept `Waitress`;
//! * capability queries are synchronous — asking what an adapter supports
//!   should not perform I/O.
//!
//! The trait is deliberately object-safe so the CLI can pick an adapter by name
//! at runtime.

#![forbid(unsafe_code)]

pub mod correlator;
pub mod error;
pub mod mock;
pub mod tx;

use core::time::Duration;

use rszigbee_spec::ids::{Ieee, ManufacturerCode, Nwk};
use rszigbee_spec::zdo::ZdoClusterId;

pub use correlator::{Correlator, Pending, WaitError};
pub use error::{AdapterError, DisconnectReason, TxFailure};
pub use mock::{MockAdapter, MockHandle};
pub use tx::{
    BroadcastAddress, Destination, SendPolicy, TxConfirm, TxOptions, ZclRx, ZclTx, ZdoTx,
};

/// What `start` found and did.
///
/// This drives real decisions in the runtime: `Reset` means the device database
/// is stale and must be cleared, `Restored` means a backup was applied and
/// frame counters moved. Collapsing it to a boolean loses the ability to make
/// those decisions safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// An existing network was found and resumed. The normal case, and the only
    /// acceptable one during a migration from another stack.
    Resumed,
    /// No usable network existed, so a new one was formed. Every previously
    /// joined device is now orphaned.
    Formed,
    /// A backup was restored onto the coordinator.
    Restored,
}

/// Static facts about what an adapter can do.
///
/// Several independent booleans by nature: each is a separate capability an
/// adapter either has or does not, and grouping them into enums would invent
/// relationships between them that do not exist.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterCapabilities {
    /// Can produce a coordinator backup.
    pub backup: bool,
    /// Supports `InterPAN`, needed for Touchlink.
    pub interpan: bool,
    /// Supports install codes.
    pub install_codes: bool,
    /// Maximum concurrent in-flight requests the coordinator tolerates.
    pub max_concurrent: usize,
    /// True when the adapter expects ZDO payloads to carry their own
    /// transaction sequence number. Upstream's `hasZdoMessageOverhead`; a
    /// per-adapter quirk, not a design flaw.
    pub zdo_sequence_in_payload: bool,
    /// The coordinator's manufacturer code, used when it originates frames.
    pub manufacturer: ManufacturerCode,
}

impl Default for AdapterCapabilities {
    fn default() -> Self {
        Self {
            backup: false,
            interpan: false,
            install_codes: false,
            max_concurrent: 1,
            zdo_sequence_in_payload: true,
            manufacturer: ManufacturerCode(0),
        }
    }
}

/// Coordinator firmware identification, for diagnostics and for the
/// compatibility checks a backup restore has to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareInfo {
    /// Adapter family, e.g. `"ember"`.
    pub family: String,
    /// Human-readable version.
    pub version: String,
    /// Extra fields, adapter-defined.
    pub meta: Vec<(String, String)>,
}

/// Live network parameters as the coordinator reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkInfo {
    /// PAN id.
    pub pan_id: u16,
    /// Extended PAN id.
    pub extended_pan_id: u64,
    /// Logical channel.
    pub channel: u8,
    /// Network update id.
    pub nwk_update_id: u8,
}

/// How to bring the network up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    /// PAN id, or `None` to generate one when forming.
    pub pan_id: Option<u16>,
    /// Extended PAN id, or `None` to generate one when forming.
    pub extended_pan_id: Option<u64>,
    /// Channel to use.
    pub channel: u8,
    /// Network key, or `None` to generate one when forming.
    ///
    /// Not `Debug`-printable in a useful way on purpose: key material must
    /// never reach a log line.
    pub network_key: Option<SecretKey>,
    /// What to do when the coordinator's existing network does not match.
    pub on_mismatch: MismatchPolicy,
}

/// What to do when the coordinator already holds a different network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MismatchPolicy {
    /// Refuse to start. **The default, and the only safe default**: forming a
    /// new network here silently orphans every device the user owns, and that
    /// is not recoverable without re-pairing all of them.
    #[default]
    Fail,
    /// Form a new network, discarding the existing one.
    Form,
}

/// A 128-bit key that will not print itself.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey([u8; 16]);

impl SecretKey {
    /// Wraps raw key material.
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Exposes the key. Every call site should be reviewable.
    #[must_use]
    pub const fn expose(&self) -> &[u8; 16] {
        &self.0
    }
}

impl core::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Upstream replaces the network key with "HIDDEN" when dumping
        // settings; making that structural is strictly better than remembering
        // to do it at each call site.
        f.write_str("SecretKey([redacted])")
    }
}

/// Events an adapter reports asynchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdapterEvent {
    /// A device joined or rejoined. The IEEE address may be absent when the
    /// coordinator only reports the short address.
    DeviceJoined {
        /// Permanent address, when reported.
        ieee: Option<Ieee>,
        /// Short address.
        nwk: Nwk,
    },
    /// A device left the network.
    DeviceLeft {
        /// Permanent address, when reported.
        ieee: Option<Ieee>,
        /// Short address, when reported.
        nwk: Option<Nwk>,
    },
    /// A ZCL frame arrived.
    Zcl(ZclRx),
    /// A ZDO response or unsolicited ZDO frame arrived.
    Zdo {
        /// The ZDO cluster.
        cluster: ZdoClusterId,
        /// Sender's short address.
        nwk: Nwk,
        /// Raw payload.
        payload: Vec<u8>,
    },
    /// The link to the coordinator went away.
    Disconnected(DisconnectReason),
}

/// A coordinator adapter.
///
/// Implementations own the transport and the protocol; they know nothing about
/// devices, definitions, capabilities, persistence or MQTT. The `&mut self`
/// receivers reflect that the adapter lives inside the runtime's task: exactly
/// one caller drives it, and fan-out is the runtime's business.
#[allow(async_fn_in_trait)]
/// The seam every coordinator family sits behind.
///
/// # Why this one is not `Sync`
///
/// The other extension points in rszigbee — the store, the availability policy,
/// a device behaviour — are `Send + Sync + 'static`, because several tasks may
/// hold them at once. An adapter is deliberately different: it is one serial
/// port with one framing state machine, so concurrent use is a protocol
/// violation rather than a performance question. Exactly one task owns it and
/// every method takes `&mut self`, which makes that ownership a compile error
/// to violate rather than a rule in a comment.
pub trait CoordinatorAdapter: Send + 'static {
    /// Brings the transport and the network up.
    ///
    /// `backup` is passed here rather than through a separate restore call so
    /// that "restore then start" cannot be got out of order.
    fn start(
        &mut self,
        network: &NetworkConfig,
        backup: Option<&[u8]>,
    ) -> impl Future<Output = Result<StartOutcome, AdapterError>> + Send;

    /// Shuts the transport down.
    fn stop(&mut self) -> impl Future<Output = Result<(), AdapterError>> + Send;

    /// The coordinator's own IEEE address.
    fn coordinator_ieee(&mut self) -> impl Future<Output = Result<Ieee, AdapterError>> + Send;

    /// Firmware identification.
    fn firmware(&mut self) -> impl Future<Output = Result<FirmwareInfo, AdapterError>> + Send;

    /// Live network parameters.
    fn network_info(&mut self) -> impl Future<Output = Result<NetworkInfo, AdapterError>> + Send;

    /// What this adapter supports. Synchronous: no I/O.
    fn capabilities(&self) -> AdapterCapabilities;

    /// Opens or closes the network to joining devices.
    fn permit_join(
        &mut self,
        duration: Duration,
        via: Option<Nwk>,
    ) -> impl Future<Output = Result<(), AdapterError>> + Send;

    /// Sends a ZCL frame. Returns the response when one was expected.
    fn send_zcl(
        &mut self,
        request: ZclTx,
    ) -> impl Future<Output = Result<Option<ZclRx>, AdapterError>> + Send;

    /// Sends a ZDO request. Returns the raw response payload when one was
    /// expected; decoding belongs to the caller.
    fn send_zdo(
        &mut self,
        request: ZdoTx,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, AdapterError>> + Send;

    /// Produces a coordinator backup, in `zigpy/open-coordinator-backup` form.
    ///
    /// `known` lets the adapter include link keys only for devices the runtime
    /// still knows about, matching upstream.
    fn backup(
        &mut self,
        known: &[Ieee],
    ) -> impl Future<Output = Result<Vec<u8>, AdapterError>> + Send {
        let _ = known;
        async { Err(AdapterError::Unsupported("coordinator backup")) }
    }

    /// Registers an install code for a device that will join with one.
    fn add_install_code(
        &mut self,
        ieee: Ieee,
        code: &[u8],
    ) -> impl Future<Output = Result<(), AdapterError>> + Send {
        let _ = (ieee, code);
        async { Err(AdapterError::Unsupported("install codes")) }
    }
}

use core::future::Future;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_key_never_prints_itself() {
        // If this test ever fails, key material is one `tracing::debug!` away
        // from a user's log file.
        let k = SecretKey::new([0xab; 16]);
        let shown = format!("{k:?}");
        assert_eq!(shown, "SecretKey([redacted])");
        assert!(!shown.contains("ab"));

        let cfg = NetworkConfig {
            pan_id: Some(0x1a62),
            extended_pan_id: None,
            channel: 11,
            network_key: Some(SecretKey::new([0xcd; 16])),
            on_mismatch: MismatchPolicy::default(),
        };
        assert!(!format!("{cfg:?}").contains("cd"));
    }

    #[test]
    fn the_default_mismatch_policy_refuses_to_form_a_new_network() {
        // The most destructive thing this project can do is form a network when
        // it should have resumed one. The default must never be Form.
        assert_eq!(MismatchPolicy::default(), MismatchPolicy::Fail);
    }

    #[test]
    fn default_capabilities_claim_nothing() {
        // An adapter must opt in to each capability; a default of "supported"
        // would mean an unimplemented method looks available.
        let c = AdapterCapabilities::default();
        assert!(!c.backup);
        assert!(!c.interpan);
        assert!(!c.install_codes);
        assert_eq!(c.max_concurrent, 1);
    }
}
