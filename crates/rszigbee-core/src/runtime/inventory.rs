//! The device table the runtime owns, and its mapping to persisted records.
//!
//! # Why the runtime resolves addresses
//!
//! A Zigbee device has two addresses: a permanent IEEE address and a short
//! network address that **changes** — on a rejoin, or when the coordinator
//! reassigns it. Frames arrive carrying the short address; everything a user
//! names, stores or configures uses the permanent one.
//!
//! Adapters differ in which they report, so translating between them is done
//! once here rather than in each adapter. That is also why a stale short
//! address is a bug worth an event: sending to one delivers to whichever device
//! now holds it, which is how a command ends up at the wrong light.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rszigbee_spec::ids::{Ieee, Nwk};

use crate::device::{DeviceInfo, DeviceKind, EndpointInfo};
use crate::reachability::{NextCheck, ReachabilityInfo};
use crate::store::{PersistedDevice, PersistedEndpoint};

/// One device as the runtime holds it: the persisted facts plus the live
/// reachability state, which is deliberately not persisted.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The device.
    pub info: DeviceInfo,
    /// Reachability facts. Not stored: a belief about whether a device is
    /// answering, restored from disk, is a belief about the past. It is cheaper
    /// and more honest to re-establish it from traffic after a restart.
    pub reachability: ReachabilityInfo,
    /// What the policy scheduled next. Not persisted, for the same reason
    /// the reachability verdict is not: a deadline from a previous process is
    /// meaningless against this one's monotonic clock.
    pub next_check: NextCheck,
    /// Fields an import did not understand, carried through untouched.
    pub passthrough: BTreeMap<String, String>,
}

/// The device table, indexed by both addresses.
#[derive(Debug, Default)]
pub struct Inventory {
    devices: BTreeMap<Ieee, Entry>,
    /// Short address to permanent address. Rebuilt on every address change,
    /// never allowed to hold two entries for one device.
    by_nwk: HashMap<Nwk, Ieee>,
}

impl Inventory {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks a device up by permanent address.
    pub fn get(&self, ieee: Ieee) -> Option<&Entry> {
        self.devices.get(&ieee)
    }

    /// Looks a device up by permanent address, mutably.
    pub fn get_mut(&mut self, ieee: Ieee) -> Option<&mut Entry> {
        self.devices.get_mut(&ieee)
    }

    /// Resolves a short address to a permanent one.
    ///
    /// Returns `None` for a device the runtime has never seen, which is a
    /// normal thing to happen: a frame can arrive from a device that joined
    /// before this installation existed.
    pub fn resolve(&self, nwk: Nwk) -> Option<Ieee> {
        self.by_nwk.get(&nwk).copied()
    }

    /// Every device, in address order.
    pub fn all(&self) -> impl Iterator<Item = &Entry> {
        self.devices.values()
    }

    /// Every device as a snapshot, in address order.
    pub fn snapshot(&self) -> Vec<DeviceInfo> {
        self.devices.values().map(|e| e.info.clone()).collect()
    }

    /// Inserts a device, replacing any existing record for the same address.
    pub fn insert(&mut self, entry: Entry) {
        let ieee = entry.info.ieee;
        // Drop any stale reverse-index entry first. Without this, a device that
        // changed short address leaves the old one pointing at it, and a frame
        // from whichever device later receives that address is attributed to
        // the wrong device.
        if let Some(previous) = self.devices.get(&ieee) {
            self.by_nwk.remove(&previous.info.nwk);
        }
        self.by_nwk.insert(entry.info.nwk, ieee);
        self.devices.insert(ieee, entry);
    }

    /// Records a new short address for a known device.
    ///
    /// Returns the previous address when it actually changed, so the caller can
    /// emit an event only for a real change rather than for every announce.
    pub fn set_nwk(&mut self, ieee: Ieee, nwk: Nwk) -> Option<Nwk> {
        let entry = self.devices.get_mut(&ieee)?;
        let previous = entry.info.nwk;
        if previous == nwk {
            return None;
        }
        entry.info.nwk = nwk;
        self.by_nwk.remove(&previous);
        self.by_nwk.insert(nwk, ieee);
        Some(previous)
    }

    /// Removes a device and its index entry.
    pub fn remove(&mut self, ieee: Ieee) -> Option<Entry> {
        let entry = self.devices.remove(&ieee)?;
        self.by_nwk.remove(&entry.info.nwk);
        Some(entry)
    }
}

/// Builds a runtime entry from a stored record.
pub fn entry_from_persisted(stored: PersistedDevice) -> Entry {
    let mut info = DeviceInfo::new(stored.ieee, stored.nwk, stored.kind);
    info.power_source = stored.power_source;
    info.interview = stored.interview;
    info.basic = stored.basic;
    info.endpoints = stored
        .endpoints
        .into_iter()
        .map(|e| EndpointInfo {
            id: e.id,
            profile: e.profile,
            device_id: e.device_id,
            input_clusters: e.input_clusters,
            output_clusters: e.output_clusters,
        })
        .collect();
    info.checkin_interval = stored
        .checkin_interval_secs
        .map(u64::from)
        .map(Duration::from_secs);
    info.last_seen = stored
        .last_seen_epoch_ms
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms));

    // `link_quality` is deliberately not restored: it describes one past frame,
    // and presenting a stale value as current is worse than presenting none.
    Entry {
        info,
        reachability: ReachabilityInfo::default(),
        next_check: NextCheck::AwaitTraffic,
        passthrough: stored.passthrough,
    }
}

/// Builds a stored record from a runtime entry.
pub fn persisted_from_entry(entry: &Entry) -> PersistedDevice {
    PersistedDevice {
        ieee: entry.info.ieee,
        nwk: entry.info.nwk,
        kind: entry.info.kind,
        power_source: entry.info.power_source,
        interview: entry.info.interview,
        basic: entry.info.basic.clone(),
        endpoints: entry
            .info
            .endpoints
            .iter()
            .map(|e| PersistedEndpoint {
                id: e.id,
                profile: e.profile,
                device_id: e.device_id,
                input_clusters: e.input_clusters.clone(),
                output_clusters: e.output_clusters.clone(),
            })
            .collect(),
        checkin_interval_secs: entry
            .info
            .checkin_interval
            .map(|d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX)),
        last_seen_epoch_ms: entry.info.last_seen.and_then(|t| {
            t.duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|d| u64::try_from(d.as_millis()).ok())
        }),
        passthrough: entry.passthrough.clone(),
    }
}

/// A minimal entry for a device seen for the first time.
///
/// The kind is unknown until an interview: a device that has only announced
/// itself has told us its addresses and nothing else, and guessing is how a
/// battery sensor ends up being polled like a mains-powered light.
pub fn new_entry(ieee: Ieee, nwk: Nwk, now: SystemTime) -> Entry {
    let mut info = DeviceInfo::new(ieee, nwk, DeviceKind::Unknown);
    info.last_seen = Some(now);
    let mut reachability = ReachabilityInfo::default();
    reachability.record_traffic(now);
    Entry {
        info,
        reachability,
        next_check: NextCheck::AwaitTraffic,
        passthrough: BTreeMap::new(),
    }
}
