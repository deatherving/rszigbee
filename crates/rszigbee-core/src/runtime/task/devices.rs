//! The device table, and what a definition says about a device.
//!
//! Joins, leaves, resolution, and the two things a definition changes about a
//! device the moment it resolves: its manufacturer-specific clusters, without
//! which its frames cannot be decoded, and its power source, which decides
//! whether it is ever probed.

use std::time::SystemTime;

use rszigbee_devices::{Definition, Extend, PowerSourceHint};
use rszigbee_spec::ids::{Ieee, Nwk};
use tracing::{debug, warn};

use super::Task;
use crate::adapter::CoordinatorAdapter;
use crate::device::{InterviewState, PowerSource};
use crate::event::{Event, LastSeenReason, LeaveReason};
use crate::runtime::definitions;
use crate::runtime::inventory;
use crate::store::ZigbeeStore;

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    pub(super) async fn on_joined(&mut self, ieee: Option<Ieee>, nwk: Nwk) {
        // Without a permanent address there is nothing to key a record on. The
        // short address is not a stable identity: it is reassigned, so storing
        // a device under one would attribute a later device's traffic to it.
        let Some(ieee) = ieee.or_else(|| self.devices.resolve(nwk)) else {
            warn!(%nwk, "a device joined without a permanent address, so it cannot be recorded");
            return;
        };

        let now = SystemTime::now();
        let known = self.devices.get(ieee).is_some();
        if known {
            if let Some(from) = self.devices.set_nwk(ieee, nwk) {
                self.emit(Event::DeviceAddressChanged {
                    ieee,
                    from,
                    to: nwk,
                });
            }
            self.emit(Event::DeviceAnnounced { ieee });
        } else {
            self.devices.insert(inventory::new_entry(ieee, nwk, now));
            self.emit(Event::DeviceJoined { ieee, nwk });
        }

        self.touch(ieee, now, LastSeenReason::Announce);
        self.persist(ieee).await;

        let needs_interview = self
            .devices
            .get(ieee)
            .is_some_and(|e| !matches!(e.info.interview, InterviewState::Successful));
        if self.interview_on_join && needs_interview {
            self.spawn_interview(ieee, None);
        }
    }

    pub(super) async fn on_left(&mut self, ieee: Option<Ieee>, nwk: Option<Nwk>) {
        let Some(ieee) = ieee.or_else(|| nwk.and_then(|n| self.devices.resolve(n))) else {
            warn!("a device left without an address the runtime could resolve");
            return;
        };
        self.devices.remove(ieee);
        if let Err(e) = self.store.delete_device(ieee).await {
            warn!(%ieee, error = %e, "could not remove the device from the store");
        }
        self.emit(Event::DeviceLeft {
            ieee,
            reason: LeaveReason::Unknown,
        });
    }

    /// Resolves the definition for a device from what the interview learned.
    ///
    /// Re-resolved rather than cached: resolution is a hash lookup plus a few
    /// comparisons, and a cache would have to be invalidated every time a
    /// device's facts changed — which is exactly when getting it wrong matters.
    pub(super) fn resolve(&self, ieee: Ieee) -> Option<&Definition> {
        let entry = self.devices.get(ieee)?;
        self.definitions
            .resolve(&definitions::device_match(&entry.info))
    }

    pub(super) async fn persist(&mut self, ieee: Ieee) {
        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let record = inventory::persisted_from_entry(entry);
        if let Err(e) = self.store.upsert_device(&record).await {
            // Not fatal: the device is still usable this run, and failing the
            // whole runtime because one write failed would be worse than
            // continuing with a warning.
            warn!(%ieee, error = %e, "could not persist the device");
        }
    }

    /// Registers the manufacturer-specific clusters a device's definition
    /// declares.
    ///
    /// Must happen before any frame from such a cluster is decoded: without
    /// the registration its attributes have no known types, so the frame
    /// decodes to nothing usable and the device looks like it reports
    /// rubbish. Registered per device, because the same id means different
    /// things to different manufacturers.
    pub(super) fn register_custom_clusters(&mut self, ieee: Ieee) {
        let Some(definition) = self.resolve(ieee) else {
            return;
        };
        let custom = definitions::custom_clusters(definition);
        if custom.is_empty() {
            return;
        }
        for def in custom {
            debug!(
                %ieee,
                cluster = def.id.0,
                name = %def.name,
                "registering a manufacturer-specific cluster for this device"
            );
            self.registry.insert_for_device(ieee, def);
        }
    }

    /// Applies definition metadata that overrides what the device reported.
    ///
    /// Currently one thing, and it matters: `forcePowerSource` exists because
    /// 26 upstream devices lie about how they are powered. A mains device that
    /// misreports as battery is never probed, so it is never noticed to have
    /// died; a battery device that misreports as mains is probed until its
    /// battery is flat. Both come from the same wrong byte.
    pub(super) async fn apply_definition_metadata(&mut self, ieee: Ieee) {
        let forced = self.resolve(ieee).and_then(|d| {
            d.extend.iter().find_map(|e| match e {
                Extend::ForcePowerSource { source } => Some(*source),
                _ => None,
            })
        });
        let Some(source) = forced else {
            return;
        };

        let (power, sleepy) = match source {
            PowerSourceHint::Mains => (PowerSource::Mains, false),
            PowerSourceHint::Dc => (PowerSource::Dc, false),
            PowerSourceHint::Battery => (PowerSource::Battery, true),
            // `PowerSourceHint` is `#[non_exhaustive]`, so a newer devices
            // crate can name a source this build does not know. Leaving the
            // reported value alone is the safe answer: overriding it with a
            // guess is how a device stops being probed.
            _ => return,
        };
        if let Some(entry) = self.devices.get_mut(ieee) {
            if entry.info.power_source == power {
                return;
            }
            debug!(
                %ieee,
                reported = ?entry.info.power_source,
                forced = ?power,
                "definition overrides the reported power source"
            );
            entry.info.power_source = power;
            entry.reachability.is_sleepy = sleepy;
        }
        self.persist(ieee).await;
    }
}
