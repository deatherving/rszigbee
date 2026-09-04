//! Persistence.
//!
//! `ZigbeeStore` holds **Zigbee domain state and nothing else.** An earlier
//! design carried generic `get_blob`/`put_blob` methods so the MQTT layer had
//! somewhere to keep its name registry and state cache. That violated the
//! "MQTT must not leak into core" rule and would have become a dumping ground
//! with no schema, no versioning and no owner.
//!
//! Layers above core own their own persistence. `rszigbee-mqtt` defines its own
//! `MqttStore`; a future HTTP or gRPC adapter would do likewise.
//!
//! A deliberate omission: there is no generic `KeyValueStore` that both this
//! trait's backends and `MqttStore` could share. It is the obvious eventual
//! answer for single-backend deployments, and it is also a generic abstraction
//! with no second caller yet. Add it when someone asks.

pub mod conformance;
#[cfg(feature = "file-store")]
pub mod file;

#[cfg(feature = "file-store")]
pub use file::FileStore;

use std::collections::BTreeMap;
use std::sync::Mutex;

use rszigbee_spec::ids::{ClusterId, EndpointId, GroupId, Ieee, Nwk, ProfileId};

use crate::adapter::SecretKey;
use crate::device::{BasicInfo, DeviceKind, InterviewState, PowerSource};

/// Persisted network identity and security material.
///
/// Losing or rolling back `frame_counter` breaks the network: replay protection
/// rejects frames with a counter it has already seen. This is the single most
/// dangerous field in the project (the README design notes).
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedNetwork {
    /// PAN id.
    pub pan_id: u16,
    /// Extended PAN id.
    ///
    /// Written as a hex string: this is routinely above 2^53, where a JSON
    /// consumer using doubles corrupts it silently.
    #[cfg_attr(feature = "file-store", serde(with = "rszigbee_spec::ids::hex_u64"))]
    pub extended_pan_id: u64,
    /// Channel.
    pub channel: u8,
    /// Network update id.
    pub nwk_update_id: u8,
    /// The coordinator this network belongs to. Checked on start: a different
    /// address means the dongle was replaced or reflashed.
    pub coordinator_ieee: Ieee,
    /// Key sequence number.
    pub key_sequence: u8,
    /// Outgoing network frame counter, plus a safety margin.
    ///
    /// Stored *ahead* of the live value on purpose. See
    /// [`FRAME_COUNTER_MARGIN`].
    pub frame_counter: u32,
    /// The network key, when the coordinator exports it.
    ///
    /// `None` is a legitimate state, not a missing field: some coordinator
    /// families decline to export it. Without the key this record describes a
    /// network but cannot recreate it on replacement hardware, which is worth
    /// knowing before the hardware needs replacing.
    ///
    /// Redacts in `Debug`. Note that it does **not** redact in the stored
    /// file: a backup that cannot restore is not a backup, so the file is the
    /// one place the key exists in the clear, and it should be treated with the
    /// same care as any other credential at rest.
    #[cfg_attr(feature = "file-store", serde(default, with = "hex_secret_key"))]
    pub network_key: Option<SecretKey>,
}

/// Serialises a [`SecretKey`] as hex.
///
/// Hex rather than a byte array so a stored network stays readable and
/// diffable, and so the field cannot be mistaken for a list of small integers.
///
/// It lives here rather than beside `SecretKey` because the key type belongs to
/// the adapter crate, which has no serde dependency and should not grow one for
/// this.
#[cfg(feature = "file-store")]
mod hex_secret_key {
    use crate::adapter::SecretKey;

    /// Serialises as a hex string, or null.
    ///
    /// `&Option<T>` rather than `Option<&T>` because serde's `with` attribute
    /// requires a reference to the field's own type; clippy's preference does
    /// not apply to a signature the derive macro dictates.
    #[allow(clippy::ref_option)]
    pub fn serialize<S: serde::Serializer>(
        key: &Option<SecretKey>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match key {
            Some(key) => {
                use core::fmt::Write as _;
                let mut hex = String::with_capacity(32);
                for byte in key.expose() {
                    let _ = write!(hex, "{byte:02x}");
                }
                serializer.serialize_str(&hex)
            }
            None => serializer.serialize_none(),
        }
    }

    /// Deserialises from a hex string, or null.
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SecretKey>, D::Error> {
        use serde::Deserialize as _;
        let Some(text) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        let bytes = text.as_bytes();
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom(format!(
                "a network key is 32 hex digits, got {}",
                bytes.len()
            )));
        }
        let mut key = [0u8; 16];
        // `as_chunks` rather than `chunks_exact`: the pair width is a constant,
        // and this states that in the type instead of leaving a runtime length
        // to reason about.
        let (pairs, _remainder) = bytes.as_chunks::<2>();
        for (slot, pair) in key.iter_mut().zip(pairs) {
            let hex = core::str::from_utf8(pair).map_err(serde::de::Error::custom)?;
            *slot = u8::from_str_radix(hex, 16).map_err(serde::de::Error::custom)?;
        }
        Ok(Some(SecretKey::new(key)))
    }
}

impl PersistedNetwork {
    /// Whether this record is missing something only the coordinator can
    /// supply.
    ///
    /// Exists for the upgrade case. A record written before the network key and
    /// the real frame counter were stored has neither, and treating "a record
    /// exists" as "the record is complete" would leave every existing
    /// installation with `frame_counter: 0` and no key permanently -- the two
    /// fields whose absence is only discovered when the hardware needs
    /// replacing or the coordinator restarts.
    ///
    /// A zero counter is the tell: a record this version writes always carries
    /// [`FRAME_COUNTER_MARGIN`] on top of the live value, so it can never be
    /// zero.
    #[must_use]
    pub const fn needs_completing(&self) -> bool {
        self.network_key.is_none() || self.frame_counter == 0
    }
}

/// How far ahead of the live value the frame counter is stored.
///
/// Every secured frame carries the outgoing counter, and every device records
/// the highest it has seen from us. A coordinator that comes back with a
/// *lower* counter has its frames dropped as replays, and the symptom is a
/// network that receives but cannot command.
///
/// Persisting on every frame would be correct and unusable -- it is a disk
/// write per message. Storing a value ahead of the real one gives the same
/// guarantee for one write: after a crash the counter resumes above anything
/// actually transmitted. The cost is skipping up to this many counter values,
/// which is free; the space is 32 bits wide and a rollover needs billions of
/// frames.
pub const FRAME_COUNTER_MARGIN: u32 = 1024;

/// One persisted endpoint.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedEndpoint {
    /// Endpoint number.
    pub id: EndpointId,
    /// Profile.
    pub profile: ProfileId,
    /// Device id.
    pub device_id: u16,
    /// Server clusters.
    pub input_clusters: Vec<ClusterId>,
    /// Client clusters.
    pub output_clusters: Vec<ClusterId>,
}

/// One persisted device.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedDevice {
    /// Address.
    pub ieee: Ieee,
    /// Last known short address.
    pub nwk: Nwk,
    /// Node type.
    pub kind: DeviceKind,
    /// Power source.
    pub power_source: PowerSource,
    /// Interview state. Always stored via `InterviewState::to_persisted`, so a
    /// crash mid-interview resumes.
    pub interview: InterviewState,
    /// `genBasic` values.
    pub basic: BasicInfo,
    /// Endpoints.
    pub endpoints: Vec<PersistedEndpoint>,
    /// Check-in interval in seconds, when known.
    pub checkin_interval_secs: Option<u32>,
    /// Last seen, as milliseconds since the Unix epoch.
    pub last_seen_epoch_ms: Option<u64>,
    /// Unrecognised fields from an import, preserved verbatim.
    ///
    /// This is what makes a `Zigbee2MQTT` import lossless and a rollback possible:
    /// anything rszigbee does not understand survives a round trip instead of
    /// being silently dropped. It also means an older rszigbee will not destroy
    /// a field added by a newer one.
    pub passthrough: BTreeMap<String, String>,
}

impl PersistedDevice {
    /// A minimal record for a device known only by address.
    #[must_use]
    pub fn new(ieee: Ieee, nwk: Nwk) -> Self {
        Self {
            ieee,
            nwk,
            kind: DeviceKind::Unknown,
            power_source: PowerSource::Unknown,
            interview: InterviewState::Pending,
            basic: BasicInfo::default(),
            endpoints: Vec::new(),
            checkin_interval_secs: None,
            last_seen_epoch_ms: None,
            passthrough: BTreeMap::new(),
        }
    }
}

/// One persisted group.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedGroup {
    /// Group id.
    pub id: GroupId,
    /// Members, as (device, endpoint) pairs.
    pub members: Vec<(Ieee, EndpointId)>,
}

/// A stored coordinator backup.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupMeta {
    /// Identifier.
    pub id: String,
    /// When it was taken, epoch milliseconds.
    pub taken_epoch_ms: u64,
    /// Which adapter family produced it.
    pub adapter: String,
    /// Which coordinator it came from.
    pub coordinator_ieee: Ieee,
}

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The backing store is unreachable.
    #[error("store unavailable: {0}")]
    Unavailable(String),
    /// Stored data could not be parsed.
    ///
    /// How this is handled depends on what was corrupt: a corrupt state cache
    /// is quarantined and startup continues, but corrupt **network identity**
    /// must stop startup, because continuing means forming a new network and
    /// orphaning every device (the README design notes).
    #[error("stored data is corrupt at {location}: {detail}")]
    Corrupt {
        /// What was being read.
        location: String,
        /// Why it failed.
        detail: String,
    },
    /// No such backup.
    #[error("backup '{0}' not found")]
    BackupNotFound(String),
    /// An underlying I/O failure.
    #[error("io error: {0}")]
    Io(String),
}

/// Persistence for Zigbee domain state.
#[allow(async_fn_in_trait)]
pub trait ZigbeeStore: Send + Sync + 'static {
    /// Loads network identity, or `None` on a fresh install.
    fn load_network(
        &self,
    ) -> impl Future<Output = Result<Option<PersistedNetwork>, StoreError>> + Send;

    /// Stores network identity.
    fn save_network(
        &self,
        network: &PersistedNetwork,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Loads every device.
    fn load_devices(&self)
    -> impl Future<Output = Result<Vec<PersistedDevice>, StoreError>> + Send;

    /// Inserts or updates one device.
    ///
    /// Per-device granularity on purpose: upstream rewrites its whole database
    /// file on any change, which at a thousand devices is a multi-megabyte
    /// fsync every time a device is heard from.
    fn upsert_device(
        &self,
        device: &PersistedDevice,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Removes one device.
    fn delete_device(&self, ieee: Ieee) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Loads every group.
    fn load_groups(&self) -> impl Future<Output = Result<Vec<PersistedGroup>, StoreError>> + Send;

    /// Inserts or updates one group.
    fn upsert_group(
        &self,
        group: &PersistedGroup,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Removes one group.
    fn delete_group(&self, id: GroupId) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Stores a coordinator backup and returns its identifier.
    fn save_backup(
        &self,
        adapter: &str,
        coordinator: Ieee,
        bytes: &[u8],
    ) -> impl Future<Output = Result<String, StoreError>> + Send;

    /// Lists stored backups, newest first.
    fn list_backups(&self) -> impl Future<Output = Result<Vec<BackupMeta>, StoreError>> + Send;

    /// Loads one backup.
    fn load_backup(&self, id: &str) -> impl Future<Output = Result<Vec<u8>, StoreError>> + Send;

    /// Flushes anything buffered.
    fn flush(&self) -> impl Future<Output = Result<(), StoreError>> + Send;
}

/// Milliseconds since the Unix epoch, saturating at zero before it.
///
/// Deliberately not a date library: a backup id has to be unique, lexically
/// sortable and legible, and a dependency for that would not pay for itself.
pub(crate) fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A shared store is still a store.
///
/// Delegating rather than requiring ownership matters for two reasons. A caller
/// that wants to inspect or back up state while the runtime is running needs a
/// second handle, and a test that wants to check what the runtime persisted
/// cannot get one if the builder consumes the only copy -- which is why the
/// frame counter went unverified for as long as it did.
impl<S: ZigbeeStore> ZigbeeStore for std::sync::Arc<S> {
    fn load_network(
        &self,
    ) -> impl Future<Output = Result<Option<PersistedNetwork>, StoreError>> + Send {
        (**self).load_network()
    }

    fn save_network(
        &self,
        network: &PersistedNetwork,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        (**self).save_network(network)
    }

    fn load_devices(
        &self,
    ) -> impl Future<Output = Result<Vec<PersistedDevice>, StoreError>> + Send {
        (**self).load_devices()
    }

    fn upsert_device(
        &self,
        device: &PersistedDevice,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        (**self).upsert_device(device)
    }

    fn delete_device(&self, ieee: Ieee) -> impl Future<Output = Result<(), StoreError>> + Send {
        (**self).delete_device(ieee)
    }

    fn load_groups(&self) -> impl Future<Output = Result<Vec<PersistedGroup>, StoreError>> + Send {
        (**self).load_groups()
    }

    fn upsert_group(
        &self,
        group: &PersistedGroup,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        (**self).upsert_group(group)
    }

    fn delete_group(&self, id: GroupId) -> impl Future<Output = Result<(), StoreError>> + Send {
        (**self).delete_group(id)
    }

    fn save_backup(
        &self,
        adapter: &str,
        coordinator: Ieee,
        bytes: &[u8],
    ) -> impl Future<Output = Result<String, StoreError>> + Send {
        (**self).save_backup(adapter, coordinator, bytes)
    }

    fn list_backups(&self) -> impl Future<Output = Result<Vec<BackupMeta>, StoreError>> + Send {
        (**self).list_backups()
    }

    fn load_backup(&self, id: &str) -> impl Future<Output = Result<Vec<u8>, StoreError>> + Send {
        (**self).load_backup(id)
    }

    fn flush(&self) -> impl Future<Output = Result<(), StoreError>> + Send {
        (**self).flush()
    }
}

/// An in-memory store. The default in tests, and always compiled.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    network: Option<PersistedNetwork>,
    devices: BTreeMap<Ieee, PersistedDevice>,
    groups: BTreeMap<u16, PersistedGroup>,
    backups: Vec<(BackupMeta, Vec<u8>)>,
}

impl MemoryStore {
    /// A new empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut MemoryInner) -> T) -> Result<T, StoreError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| StoreError::Unavailable("memory store poisoned".into()))?;
        Ok(f(&mut guard))
    }
}

// `ZigbeeStore` is async because a real backend awaits a disk or a socket.
// This one is a `Mutex` around some maps, so nothing awaits, and the signatures
// are still fixed by the trait.
#[allow(clippy::unused_async_trait_impl)]
impl ZigbeeStore for MemoryStore {
    async fn load_network(&self) -> Result<Option<PersistedNetwork>, StoreError> {
        self.with(|i| i.network.clone())
    }

    async fn save_network(&self, network: &PersistedNetwork) -> Result<(), StoreError> {
        self.with(|i| i.network = Some(network.clone()))
    }

    async fn load_devices(&self) -> Result<Vec<PersistedDevice>, StoreError> {
        self.with(|i| i.devices.values().cloned().collect())
    }

    async fn upsert_device(&self, device: &PersistedDevice) -> Result<(), StoreError> {
        self.with(|i| {
            i.devices.insert(device.ieee, device.clone());
        })
    }

    async fn delete_device(&self, ieee: Ieee) -> Result<(), StoreError> {
        self.with(|i| {
            i.devices.remove(&ieee);
        })
    }

    async fn load_groups(&self) -> Result<Vec<PersistedGroup>, StoreError> {
        self.with(|i| i.groups.values().cloned().collect())
    }

    async fn upsert_group(&self, group: &PersistedGroup) -> Result<(), StoreError> {
        self.with(|i| {
            i.groups.insert(group.id.0, group.clone());
        })
    }

    async fn delete_group(&self, id: GroupId) -> Result<(), StoreError> {
        self.with(|i| {
            i.groups.remove(&id.0);
        })
    }

    async fn save_backup(
        &self,
        adapter: &str,
        coordinator: Ieee,
        bytes: &[u8],
    ) -> Result<String, StoreError> {
        self.with(|i| {
            let id = format!("backup-{}", i.backups.len() + 1);
            i.backups.push((
                BackupMeta {
                    id: id.clone(),
                    // A real timestamp, not zero: "restore the backup from
                    // before the outage" has to be answerable against this
                    // backend too, and a caller cannot tell which backend it
                    // has. Asserted by the conformance suite.
                    taken_epoch_ms: epoch_millis(),
                    adapter: adapter.to_owned(),
                    coordinator_ieee: coordinator,
                },
                bytes.to_vec(),
            ));
            id
        })
    }

    async fn list_backups(&self) -> Result<Vec<BackupMeta>, StoreError> {
        self.with(|i| i.backups.iter().rev().map(|(m, _)| m.clone()).collect())
    }

    async fn load_backup(&self, id: &str) -> Result<Vec<u8>, StoreError> {
        let found = self.with(|i| {
            i.backups
                .iter()
                .find(|(m, _)| m.id == id)
                .map(|(_, b)| b.clone())
        })?;
        found.ok_or_else(|| StoreError::BackupNotFound(id.to_owned()))
    }

    async fn flush(&self) -> Result<(), StoreError> {
        // Nothing is buffered: every method writes straight into the map
        // behind the mutex. A no-op rather than a counter, because the counter
        // it used to keep was never read by anything.
        Ok(())
    }
}

use core::future::Future;

#[cfg(test)]
mod tests {
    use super::*;

    fn network() -> PersistedNetwork {
        PersistedNetwork {
            pan_id: 0x1a62,
            extended_pan_id: 0xdddd_dddd_dddd_dddd,
            channel: 11,
            nwk_update_id: 0,
            coordinator_ieee: Ieee::new(0x0012_4b00_2218_9abc),
            key_sequence: 0,
            frame_counter: 12_345,
            network_key: Some(SecretKey::new([0xab; 16])),
        }
    }

    #[tokio::test]
    async fn a_fresh_store_has_no_network_which_is_not_an_error() {
        let s = MemoryStore::new();
        assert_eq!(s.load_network().await.unwrap(), None);
        assert!(s.load_devices().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn network_identity_round_trips_including_the_frame_counter() {
        // The frame counter is the field that breaks a network if it is lost or
        // rolled back, so it gets an explicit assertion of its own.
        let s = MemoryStore::new();
        s.save_network(&network()).await.unwrap();
        let got = s.load_network().await.unwrap().unwrap();
        assert_eq!(got, network());
        assert_eq!(got.frame_counter, 12_345);
    }

    #[tokio::test]
    async fn devices_are_written_and_removed_individually() {
        let s = MemoryStore::new();
        let a = PersistedDevice::new(Ieee::new(1), Nwk::new(10));
        let b = PersistedDevice::new(Ieee::new(2), Nwk::new(20));
        s.upsert_device(&a).await.unwrap();
        s.upsert_device(&b).await.unwrap();
        assert_eq!(s.load_devices().await.unwrap().len(), 2);

        s.delete_device(Ieee::new(1)).await.unwrap();
        let left = s.load_devices().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left.first().map(|d| d.ieee), Some(Ieee::new(2)));
    }

    #[tokio::test]
    async fn upsert_replaces_rather_than_duplicates() {
        let s = MemoryStore::new();
        let mut d = PersistedDevice::new(Ieee::new(1), Nwk::new(10));
        s.upsert_device(&d).await.unwrap();
        d.nwk = Nwk::new(99);
        s.upsert_device(&d).await.unwrap();
        let all = s.load_devices().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all.first().map(|x| x.nwk), Some(Nwk::new(99)));
    }

    #[tokio::test]
    async fn unrecognised_imported_fields_survive_a_round_trip() {
        // This is what makes a Zigbee2MQTT import lossless and a rollback safe.
        let s = MemoryStore::new();
        let mut d = PersistedDevice::new(Ieee::new(1), Nwk::new(10));
        d.passthrough
            .insert("useOnOffTransition".into(), "true".into());
        d.passthrough.insert("someFutureField".into(), "42".into());
        s.upsert_device(&d).await.unwrap();

        let back = s.load_devices().await.unwrap();
        let got = back.first().expect("device");
        assert_eq!(
            got.passthrough
                .get("useOnOffTransition")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(got.passthrough.len(), 2);
    }

    #[tokio::test]
    async fn interview_state_is_stored_in_its_resumable_form() {
        let s = MemoryStore::new();
        let mut d = PersistedDevice::new(Ieee::new(1), Nwk::new(10));
        d.interview = InterviewState::InProgress.to_persisted();
        s.upsert_device(&d).await.unwrap();
        assert_eq!(
            s.load_devices().await.unwrap().first().map(|x| x.interview),
            Some(InterviewState::Pending)
        );
    }

    #[tokio::test]
    async fn backups_are_kept_as_history_newest_first() {
        let s = MemoryStore::new();
        let coord = Ieee::new(0x0012_4b00_2218_9abc);
        let first = s.save_backup("ember", coord, b"one").await.unwrap();
        let second = s.save_backup("ember", coord, b"two").await.unwrap();

        let list = s.list_backups().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.first().map(|m| m.id.clone()), Some(second.clone()));

        // A restore needs the exact bytes of a specific backup, not the latest.
        assert_eq!(s.load_backup(&first).await.unwrap(), b"one");
        assert_eq!(s.load_backup(&second).await.unwrap(), b"two");
    }

    #[tokio::test]
    async fn a_missing_backup_is_a_typed_error() {
        let s = MemoryStore::new();
        assert!(matches!(
            s.load_backup("nope").await,
            Err(StoreError::BackupNotFound(_))
        ));
    }

    #[tokio::test]
    async fn groups_round_trip() {
        let s = MemoryStore::new();
        s.upsert_group(&PersistedGroup {
            id: GroupId(7),
            members: vec![(Ieee::new(1), EndpointId(1))],
        })
        .await
        .unwrap();
        assert_eq!(s.load_groups().await.unwrap().len(), 1);
        s.delete_group(GroupId(7)).await.unwrap();
        assert!(s.load_groups().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_memory_store_conforms() {
        // The same suite runs against FileStore. Two backends tested only
        // separately are two backends free to drift apart.
        conformance::assert_conforms(&MemoryStore::new()).await;
    }
}
