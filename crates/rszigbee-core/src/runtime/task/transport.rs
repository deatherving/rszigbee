//! Talking to the coordinator.
//!
//! Everything that puts a frame on the radio, and everything that waits for one
//! to come back. Grouped because they share an invariant that is easy to break
//! apart: a sequence number is allocated *immediately* before the payload that
//! carries it is built, so the value on the wire is always the value being
//! waited on. Spread across the file, that pairing was one careless edit away
//! from correlating on a number that was never sent.

use std::time::Instant;

use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId, Ieee, Nwk};
use rszigbee_spec::zcl::ZclValue;
use rszigbee_spec::zdo::ZdoClusterId;
use tokio::sync::oneshot;

use super::{PendingZcl, PendingZdo, Task};
use crate::adapter::{AdapterError, CoordinatorAdapter};
use crate::runtime::{RuntimeError, ZDO_TIMEOUT, decode, encode};
use crate::store::ZigbeeStore;

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    pub(super) async fn send_zdo(
        &mut self,
        ieee: Ieee,
        cluster: ZdoClusterId,
        build: Box<dyn FnOnce(u8) -> Vec<u8> + Send>,
        reply: oneshot::Sender<Result<Vec<u8>, RuntimeError>>,
    ) {
        let Some(nwk) = self.devices.get(ieee).map(|e| e.info.nwk) else {
            let _ = reply.send(Err(RuntimeError::UnknownDevice(ieee)));
            return;
        };

        // Allocated here, immediately before the payload is built with it, so
        // the value that goes on the wire is the value we correlate on.
        self.zdo_sequence = self.zdo_sequence.wrapping_add(1);
        let sequence = self.zdo_sequence;
        let payload = build(sequence);

        let tx = crate::adapter::ZdoTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            cluster,
            payload,
            options: crate::adapter::TxOptions::default(),
        };

        match self.adapter.send_zdo(tx).await {
            // Some adapters answer inline; most deliver the response as an
            // event. Both are supported rather than one being assumed.
            Ok(Some(payload)) => {
                let _ = reply.send(Ok(payload));
            }
            Ok(None) => {
                self.pending_zdo.insert(
                    (cluster.response(), sequence),
                    PendingZdo {
                        reply,
                        ieee,
                        deadline: Instant::now() + ZDO_TIMEOUT,
                    },
                );
            }
            Err(e) => {
                let _ = reply.send(Err(e.into()));
            }
        }
    }

    /// Reads attributes, correlating the response by transaction sequence.
    pub(super) async fn zcl_read(
        &mut self,
        ieee: Ieee,
        endpoint: EndpointId,
        cluster: ClusterId,
        attributes: &[AttrId],
        reply: oneshot::Sender<Result<Vec<(u16, ZclValue)>, RuntimeError>>,
    ) {
        let Some(nwk) = self.devices.get(ieee).map(|e| e.info.nwk) else {
            let _ = reply.send(Err(RuntimeError::UnknownDevice(ieee)));
            return;
        };

        self.zcl_sequence = self.zcl_sequence.wrapping_add(1);
        let tsn = self.zcl_sequence;

        let tx = crate::adapter::ZclTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            endpoint,
            source_endpoint: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            cluster,
            frame: encode::read_attributes(tsn, attributes),
            options: crate::adapter::TxOptions {
                expect_response: true,
                ..crate::adapter::TxOptions::default()
            },
        };

        match self.adapter.send_zcl(tx).await {
            // Some adapters answer inline; the Ember one delivers the response
            // as an event. Both are handled rather than one being assumed.
            Ok(Some(rx)) => {
                let decoded = decode::zcl_message(&self.registry, ieee, &rx)
                    .ok()
                    .and_then(|m| match m.kind {
                        crate::event::ZclMessageKind::Attributes(a) => Some(a),
                        _ => None,
                    })
                    .unwrap_or_default();
                let _ = reply.send(Ok(decoded));
            }
            Ok(None) => {
                self.pending_zcl.insert(
                    (cluster, tsn),
                    PendingZcl {
                        reply,
                        ieee,
                        deadline: Instant::now() + ZDO_TIMEOUT,
                    },
                );
            }
            Err(e) => {
                let _ = reply.send(Err(e.into()));
            }
        }
    }

    /// Fails ZDO requests whose deadline passed or whose caller went away.
    ///
    /// Without this the map grows for every device that never answers, which
    /// is a slow leak in exactly the deployment where devices do not answer.
    pub(super) fn expire_pending_zdo(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending_zdo
            .iter()
            .filter(|(_, p)| p.deadline <= now || p.reply.is_closed())
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            if let Some(pending) = self.pending_zdo.remove(&key) {
                let _ = pending.reply.send(Err(RuntimeError::ZdoTimeout {
                    ieee: pending.ieee,
                    timeout: ZDO_TIMEOUT,
                }));
            }
        }

        let stale: Vec<_> = self
            .pending_zcl
            .iter()
            .filter(|(_, p)| p.deadline <= now || p.reply.is_closed())
            .map(|(k, _)| *k)
            .collect();
        for key in stale {
            if let Some(pending) = self.pending_zcl.remove(&key) {
                let _ = pending.reply.send(Err(RuntimeError::ZclTimeout {
                    ieee: pending.ieee,
                    timeout: ZDO_TIMEOUT,
                }));
            }
        }
    }

    /// Sends a `Bind_req` and waits for its response.
    pub(super) async fn bind(
        &mut self,
        ieee: Ieee,
        nwk: Nwk,
        endpoint: EndpointId,
        cluster: ClusterId,
        coordinator: Ieee,
    ) -> Result<(), AdapterError> {
        self.zdo_sequence = self.zdo_sequence.wrapping_add(1);
        let sequence = self.zdo_sequence;
        let payload = rszigbee_spec::zdo::encode_bind_req(
            sequence,
            ieee,
            endpoint,
            cluster,
            coordinator,
            // The coordinator's own application endpoint, which is where the
            // Ember adapter registers its clusters.
            EndpointId(1),
        );
        self.adapter
            .send_zdo(crate::adapter::ZdoTx {
                dest: crate::adapter::Destination::Unicast { ieee, nwk },
                cluster: ZdoClusterId::BIND_REQ,
                payload,
                options: crate::adapter::TxOptions::default(),
            })
            .await
            .map(|_| ())
    }

    /// Sends a `configureReporting` command.
    pub(super) async fn configure_reporting(
        &mut self,
        ieee: Ieee,
        nwk: Nwk,
        endpoint: EndpointId,
        cluster: ClusterId,
        records: &[encode::ReportRecord],
    ) -> Result<(), AdapterError> {
        self.zcl_sequence = self.zcl_sequence.wrapping_add(1);
        let frame = encode::configure_reporting(self.zcl_sequence, records)
            .map_err(|e| AdapterError::Transport(format!("cannot encode reporting config: {e}")))?;
        self.adapter
            .send_zcl(crate::adapter::ZclTx {
                dest: crate::adapter::Destination::Unicast { ieee, nwk },
                endpoint,
                source_endpoint: EndpointId(1),
                profile: rszigbee_spec::ids::ProfileId::HA,
                cluster,
                frame,
                options: crate::adapter::TxOptions::default(),
            })
            .await
            .map(|_| ())
    }
}
