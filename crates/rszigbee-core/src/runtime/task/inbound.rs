//! Received frames, and what they become.
//!
//! Three destinations, and which one a frame takes is decided here rather than
//! by the consumer: an attribute report becomes state, a cluster-specific
//! command becomes an action, and a Tuya datapoint report becomes state through
//! its own table. A frame that cannot be attributed or decoded still surfaces,
//! because it is the only evidence anyone has for modelling whatever sent it.

use std::time::SystemTime;

use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee, Nwk};
use rszigbee_spec::zcl::ZclValue;
use rszigbee_spec::zdo::ZdoClusterId;
use tracing::debug;

use super::Task;
use crate::adapter::CoordinatorAdapter;
use crate::event::{Event, LastSeenReason};
use crate::runtime::{decode, definitions, tuya};
use crate::store::ZigbeeStore;

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    /// Which endpoint to report an event against, if any.
    ///
    /// `None` on a single-endpoint device, because there the endpoint is noise
    /// — every event would carry the same `1`. On a multi-endpoint device it is
    /// the difference between two identical capabilities, so it has to be
    /// there. Stated once rather than at each call site: a rule spelled out
    /// three times is a rule that can end up meaning three things.
    fn reported_endpoint(&self, ieee: Ieee, endpoint: EndpointId) -> Option<EndpointId> {
        self.devices
            .get(ieee)
            .is_some_and(|e| e.info.endpoints.len() > 1)
            .then_some(endpoint)
    }

    pub(super) fn on_zcl(&mut self, rx: crate::adapter::ZclRx) {
        // Everything downstream is keyed by permanent address, so a frame we
        // cannot attribute is a frame we cannot report. It is logged rather
        // than dropped silently, because the fix is a ZDO address lookup and
        // this is the evidence that one is needed.
        let Some(ieee) = rx.ieee.or_else(|| self.devices.resolve(rx.nwk)) else {
            debug!(
                nwk = rx.nwk.raw(),
                cluster = rx.cluster.0,
                "a frame arrived from a short address with no known device"
            );
            return;
        };

        // A frame from a known device whose short address moved is the cheapest
        // place to notice a rejoin: it arrives before any announce would.
        if let Some(from) = self.devices.set_nwk(ieee, rx.nwk) {
            self.emit(Event::DeviceAddressChanged {
                ieee,
                from,
                to: rx.nwk,
            });
        }
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.info.link_quality = rx.link_quality.or(entry.info.link_quality);
        }

        // A frame carrying the transaction sequence of an outstanding read is
        // that read's answer. Checked before the report path, so a read does
        // not also surface as an unsolicited attribute report.
        if let Ok(frame) = rszigbee_spec::zcl::frame::ZclFrame::decode(&rx.frame)
            && let Some(pending) = self.pending_zcl.remove(&(rx.cluster, frame.header.tsn))
        {
            let decoded = decode::zcl_message(&self.registry, ieee, &rx)
                .ok()
                .and_then(|m| match m.kind {
                    crate::event::ZclMessageKind::Attributes(a) => Some(a),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = pending.reply.send(Ok(decoded));
            self.touch(ieee, SystemTime::now(), LastSeenReason::Message);
            return;
        }

        // Decoded, then reported. A frame that will not decode still moves
        // `last_seen` and still produces an event, because it is proof the
        // device is alive and it is the only evidence anyone has for adding
        // support for it.
        match decode::zcl_message(&self.registry, ieee, &rx) {
            Ok(message) => {
                self.touch(ieee, SystemTime::now(), LastSeenReason::Message);
                // The sensor path's last link: an attribute report becomes
                // typed capability state, so a caller sees `temperature: 21.37`
                // and not a cluster id and an integer.
                match &message.kind {
                    crate::event::ZclMessageKind::Attributes(attributes) => {
                        self.publish_state(ieee, rx.endpoint, rx.cluster, attributes);
                    }
                    // A button press. Emitted as an action, never folded into
                    // state: it is momentary, and a state object that carries
                    // it has to have it excluded again on the way out.
                    crate::event::ZclMessageKind::Command { id, params, .. } => {
                        // Tuya reports arrive as cluster-specific commands on
                        // the manufacturer cluster rather than as attribute
                        // reports, which is why they need their own branch:
                        // nothing in the standard path would ever see them.
                        if rx.cluster == rszigbee_spec::tuya::CLUSTER
                            && rszigbee_spec::tuya::is_report(rszigbee_spec::ids::CommandId(*id))
                        {
                            self.publish_tuya(ieee, rx.endpoint, &rx.frame);
                        } else {
                            self.publish_action(ieee, rx.endpoint, rx.cluster, *id, params);
                        }
                    }
                    crate::event::ZclMessageKind::DefaultResponse { .. } => {}
                }
                self.emit(Event::ZclMessage(message));
            }
            Err(reason) => {
                self.touch(
                    ieee,
                    SystemTime::now(),
                    LastSeenReason::MessageWithoutPayload,
                );
                self.emit(Event::UnparsedFrame {
                    ieee,
                    endpoint: rx.endpoint,
                    cluster: rx.cluster,
                    raw: rx.frame,
                    reason,
                });
            }
        }
    }

    pub(super) fn on_zdo(&mut self, cluster: ZdoClusterId, nwk: Nwk, payload: Vec<u8>) {
        // Correlate first: a waiting caller wants the payload whether or not
        // the sender is a device we have a record of.
        if let Some(&sequence) = payload.first()
            && let Some(pending) = self.pending_zdo.remove(&(cluster, sequence))
        {
            let _ = pending.reply.send(Ok(payload.clone()));
        }

        if let Some(ieee) = self.devices.resolve(nwk) {
            self.touch(ieee, SystemTime::now(), LastSeenReason::Message);
        }

        self.emit(Event::ZdoResponse {
            nwk,
            cluster,
            payload,
        });
    }

    /// Turns reported attributes into a capability state delta.
    ///
    /// Only attributes the definition actually models produce state. An
    /// unmodelled attribute is left to [`Event::ZclMessage`], which still
    /// carries it: inventing a capability name would put junk into a caller's
    /// state, and dropping the frame would lose the only evidence anyone has
    /// for modelling it.
    ///
    /// The event carries the delta, never a merged snapshot. Publishing a
    /// snapshot is a compatibility behaviour that belongs to the MQTT layer.
    pub(super) fn publish_state(
        &mut self,
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        attributes: &[(u16, ZclValue)],
    ) {
        let Some(definition) = self.resolve(ieee) else {
            return;
        };

        let mut changes = crate::state::StateChanges::new();
        for (attribute, value) in attributes {
            if let Some((capability, state)) =
                definitions::report_to_state(definition, cluster, *attribute, value)
            {
                changes.set(capability, state);
            }
        }
        if changes.is_empty() {
            return;
        }

        self.emit(Event::StateChanged {
            ieee,
            endpoint: self.reported_endpoint(ieee, endpoint),
            changes,
        });
    }

    /// Emits an action for a received cluster command, if one is named.
    pub(super) fn publish_action(
        &mut self,
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        id: u8,
        params: &[(String, ZclValue)],
    ) {
        let Some(definition) = self.resolve(ieee) else {
            return;
        };
        let Some((capability, action)) =
            definitions::command_to_action(definition, cluster, id, params)
        else {
            return;
        };
        self.emit(Event::Action {
            ieee,
            endpoint: self.reported_endpoint(ieee, endpoint),
            capability,
            action,
        });
    }

    /// Decodes a Tuya datapoint report into capability state.
    ///
    /// Decoded from the raw frame rather than from the already-decoded
    /// parameters, because the datapoint list is a composite type the ZCL
    /// registry cannot describe — it is exactly the `1011` synthetic type the
    /// cluster table marks unencodable.
    pub(super) fn publish_tuya(&mut self, ieee: Ieee, endpoint: EndpointId, frame: &[u8]) {
        let Ok(parsed) = rszigbee_spec::zcl::frame::ZclFrame::decode(frame) else {
            return;
        };
        let (_, datapoints) = match rszigbee_spec::tuya::decode(parsed.payload) {
            Ok(decoded) => decoded,
            Err(reason) => {
                // Surfaced rather than dropped: a malformed Tuya frame is
                // usually a datapoint nobody has modelled, and the bytes are
                // the only evidence for modelling it.
                debug!(%ieee, %reason, "a Tuya payload could not be decoded");
                return;
            }
        };
        if datapoints.is_empty() {
            return;
        }

        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let Some(definition) = self
            .definitions
            .resolve(&definitions::device_match(&entry.info))
        else {
            return;
        };
        let changes =
            tuya::datapoints_to_state(definition, &datapoints, &entry.info, &self.behaviors);
        if changes.is_empty() {
            // The device reported datapoints the table does not name. Worth
            // seeing, because it is the signal that the definition is
            // incomplete for this firmware revision.
            debug!(
                %ieee,
                datapoints = ?datapoints.iter().map(|d| d.dp).collect::<Vec<_>>(),
                "Tuya datapoints arrived that the definition does not name"
            );
            return;
        }

        self.emit(Event::StateChanged {
            ieee,
            endpoint: self.reported_endpoint(ieee, endpoint),
            changes,
        });
    }
}
