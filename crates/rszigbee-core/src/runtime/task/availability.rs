//! Whether a device is still answering.
//!
//! The facts live here and the policy is injected, which is what lets an
//! embedded application have availability without running MQTT. This module
//! only records what happened and does what the policy scheduled; it never
//! decides what silence means.

use std::time::{Instant, SystemTime};

use rszigbee_spec::ids::{EndpointId, Ieee};

use super::Task;
use crate::adapter::{AdapterError, CoordinatorAdapter};
use crate::event::{Event, LastSeenReason};
use crate::reachability::{Evidence, NextCheck, ProbeResult, ReachabilityContext};
use crate::runtime::encode;
use crate::store::ZigbeeStore;

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    /// Records that a frame arrived, and re-assesses reachability.
    pub(super) fn touch(&mut self, ieee: Ieee, at: SystemTime, reason: LastSeenReason) {
        let Some(entry) = self.devices.get_mut(ieee) else {
            return;
        };
        entry.info.last_seen = Some(at);
        entry.reachability.record_traffic(at);
        self.emit(Event::LastSeenChanged { ieee, at, reason });
        self.assess(ieee, Evidence::Traffic);
    }

    pub(super) fn record_tx(
        &mut self,
        ieee: Ieee,
        at: Instant,
        result: Result<(), crate::adapter::TxFailure>,
    ) {
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.reachability.record_tx(at, result);
        }
        self.assess(
            ieee,
            match result {
                Ok(()) => Evidence::CommandAcked,
                Err(failure) => Evidence::CommandFailed(failure),
            },
        );
    }

    /// Asks the policy for a verdict and emits a change only if there was one.
    pub(super) fn assess(&mut self, ieee: Ieee, evidence: Evidence) {
        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let assessment = self.reachability.assess(&ReachabilityContext {
            device: &entry.info,
            current: &entry.reachability,
            now: Instant::now(),
            wall_now: SystemTime::now(),
        });

        let previous = entry.reachability.state;
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.reachability.state = assessment.verdict;
            entry.next_check = assessment.next;
        }
        if previous != assessment.verdict {
            self.emit(Event::ReachabilityChanged {
                ieee,
                from: previous,
                to: assessment.verdict,
                evidence,
            });
        }
    }

    /// Runs whatever the policy scheduled and is now due.
    pub(super) async fn run_due_reachability(&mut self) {
        let now = Instant::now();
        let due: Vec<(Ieee, NextCheck)> = self
            .devices
            .all()
            .filter_map(|e| match e.next_check {
                NextCheck::Probe { at, .. } if at <= now => Some((e.info.ieee, e.next_check)),
                NextCheck::Reassess { at } if at <= now => Some((e.info.ieee, e.next_check)),
                _ => None,
            })
            .collect();

        for (ieee, next) in due {
            match next {
                NextCheck::Probe { allow_recovery, .. } => {
                    self.probe(ieee, allow_recovery).await;
                }
                // No probe: the device sleeps and cannot answer on demand, but
                // its silence still eventually means something.
                NextCheck::Reassess { .. } => self.assess(ieee, Evidence::Elapsed),
                NextCheck::AwaitTraffic => {}
            }
        }
    }

    /// Probes a device by reading `genBasic.zclVersion`.
    ///
    /// Every Zigbee device implements this attribute, so a probe needs no
    /// definition and cannot be refused for being unsupported. What is being
    /// tested is whether the device answers at all, not what it answers.
    pub(super) async fn probe(&mut self, ieee: Ieee, allow_recovery: bool) {
        let Some(entry) = self.devices.get(ieee) else {
            return;
        };
        let nwk = entry.info.nwk;
        let endpoint = entry.info.endpoints.first().map_or(EndpointId(1), |e| e.id);

        let tx = crate::adapter::ZclTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            endpoint,
            source_endpoint: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            cluster: encode::PROBE_CLUSTER,
            frame: encode::probe(0),
            options: crate::adapter::TxOptions {
                expect_response: true,
                // Route repair is slow and noisy, and a probe is expected to
                // fail sometimes. The policy decides when it is worth it.
                disable_recovery: !allow_recovery,
                ..crate::adapter::TxOptions::default()
            },
        };

        let result = self.adapter.send_zcl(tx).await;
        let at = Instant::now();
        let outcome = match result {
            Ok(_) => ProbeResult::Answered,
            Err(AdapterError::Tx(failure)) => ProbeResult::Failed(failure),
            // Anything below the ZCL layer is not evidence about the device, so
            // it is reported as the closest thing that is: no acknowledgement.
            Err(_) => ProbeResult::Failed(crate::adapter::TxFailure::NoAck),
        };
        if let Some(entry) = self.devices.get_mut(ieee) {
            entry.reachability.record_probe(at, outcome);
        }
        self.assess(ieee, Evidence::Probe(outcome));
    }
}
