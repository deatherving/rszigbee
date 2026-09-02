//! Interviewing a device over ZDO.
//!
//! An interview asks a device what it is: its node descriptor, which endpoints
//! it has, and what each endpoint speaks. Everything above this — definition
//! matching, capabilities, the MQTT `exposes` shape — is derived from what is
//! learned here, so an interview that quietly half-fails produces a device that
//! quietly half-works.
//!
//! # A partial interview is a result, not an error
//!
//! Real devices refuse steps. Some answer the node descriptor and nothing else;
//! some answer once and never again; a battery device may be asleep for most of
//! it. zigbee-herdsman carries a quirk table for exactly this, and treating any
//! refusal as fatal would discard devices that upstream supports.
//!
//! So each step records what it got, failures are collected rather than
//! propagated, and the outcome says how far it got. The interview state only
//! becomes [`InterviewState::Successful`] when the endpoints are actually known,
//! because that is the part everything downstream needs.

use rszigbee_spec::ids::{EndpointId, Ieee, Nwk};
use rszigbee_spec::zdo::{self, ZdoClusterId};
use tracing::{debug, warn};

use super::{RuntimeError, Zigbee};
use crate::device::{DeviceKind, EndpointInfo, InterviewState, PowerSource};
use crate::event::InterviewStep;

/// Progress an interview reports back to the runtime task.
///
/// The interview runs as its own task and cannot touch the device table, so
/// every fact it learns travels through here. That is a feature: there is one
/// place where interview results are applied, and it is on the loop.
#[derive(Debug, Clone)]
pub(crate) enum InterviewUpdate {
    /// The interview began.
    Started,
    /// The node descriptor was read.
    NodeDescriptor {
        /// Node type.
        kind: DeviceKind,
        /// Power source.
        power: PowerSource,
        /// Whether the device sleeps, from `rx_on_when_idle`.
        sleepy: bool,
    },
    /// One step completed or failed; either way it is worth reporting.
    Step(InterviewStep),
    /// The interview finished.
    Finished(Box<InterviewOutcome>),
}

/// What an interview learned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InterviewOutcome {
    /// The state the device ended in.
    pub state: InterviewState,
    /// Steps that completed.
    pub completed: Vec<InterviewStep>,
    /// Steps that did not, and why. Collected rather than returned, so one
    /// refusal does not hide what the other steps found.
    pub failures: Vec<(InterviewStep, String)>,
    /// Endpoints discovered.
    pub endpoints: Vec<EndpointInfo>,
}

impl InterviewOutcome {
    /// Whether the interview learned enough for the device to be usable:
    /// at least one endpoint with its clusters.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        !self.endpoints.is_empty()
    }
}

/// Runs an interview and applies what it learns to the device record.
///
/// # Errors
///
/// Only fails if the runtime stopped mid-interview. A device that refuses
/// every step produces an outcome describing that, not an error.
pub async fn run(zigbee: &Zigbee, ieee: Ieee) -> Result<InterviewOutcome, RuntimeError> {
    zigbee
        .interview_update(ieee, InterviewUpdate::Started)
        .await?;

    let mut outcome = InterviewOutcome {
        state: InterviewState::Failed,
        completed: Vec::new(),
        failures: Vec::new(),
        endpoints: Vec::new(),
    };

    node_descriptor(zigbee, ieee, &mut outcome).await?;
    let endpoints = active_endpoints(zigbee, ieee, &mut outcome).await?;
    simple_descriptors(zigbee, ieee, endpoints, &mut outcome).await?;

    // Endpoints decide the verdict, because they are what everything
    // downstream needs. A node descriptor on its own does not make a device
    // usable, so calling that a success would mean reporting devices as ready
    // when nothing can be done with them.
    outcome.state = if outcome.is_usable() {
        InterviewState::Successful
    } else {
        InterviewState::Failed
    };

    if outcome.failures.is_empty() {
        debug!(%ieee, endpoints = outcome.endpoints.len(), "interview finished");
    } else {
        warn!(
            %ieee,
            endpoints = outcome.endpoints.len(),
            failures = ?outcome.failures,
            "interview finished with failures"
        );
    }

    zigbee
        .interview_update(ieee, InterviewUpdate::Finished(Box::new(outcome.clone())))
        .await?;
    Ok(outcome)
}

/// Reads the node descriptor: what kind of device this is, and whether it
/// sleeps.
///
/// The two facts that matter most come from here and nowhere else.
/// `rx_on_when_idle` decides whether the device can ever be probed on demand,
/// and mains power decides whether silence means anything. Guessing either
/// produces a device that is permanently reported offline, or one that is never
/// reported offline at all.
async fn node_descriptor(
    zigbee: &Zigbee,
    ieee: Ieee,
    outcome: &mut InterviewOutcome,
) -> Result<(), RuntimeError> {
    match zigbee
        .zdo(ieee, ZdoClusterId::NODE_DESC_REQ, move |seq| {
            zdo::encode_node_desc_req(seq, Nwk::COORDINATOR)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|payload| zdo::decode_node_desc_rsp(&payload).map_err(|e| e.to_string()))
    {
        Ok(descriptor) => {
            let kind = match descriptor.logical_type {
                zdo::LogicalType::Coordinator => DeviceKind::Coordinator,
                zdo::LogicalType::Router => DeviceKind::Router,
                zdo::LogicalType::EndDevice => DeviceKind::EndDevice,
                zdo::LogicalType::Reserved(_) => DeviceKind::Unknown,
            };
            let power = if descriptor.mains_powered() {
                PowerSource::Mains
            } else {
                PowerSource::Battery
            };
            zigbee
                .interview_update(
                    ieee,
                    InterviewUpdate::NodeDescriptor {
                        kind,
                        power,
                        sleepy: !descriptor.rx_on_when_idle(),
                    },
                )
                .await?;
            outcome.completed.push(InterviewStep::NodeDescriptor);
        }
        Err(detail) => outcome
            .failures
            .push((InterviewStep::NodeDescriptor, detail)),
    }
    zigbee
        .interview_update(ieee, InterviewUpdate::Step(InterviewStep::NodeDescriptor))
        .await
}

/// Enumerates the device's endpoints.
///
/// An empty list is not the same as a failure, and both are survivable. Which
/// one happened is recorded in `outcome.failures`.
async fn active_endpoints(
    zigbee: &Zigbee,
    ieee: Ieee,
    outcome: &mut InterviewOutcome,
) -> Result<Vec<EndpointId>, RuntimeError> {
    let endpoints = match zigbee
        .zdo(ieee, ZdoClusterId::ACTIVE_EP_REQ, move |seq| {
            zdo::encode_active_ep_req(seq, Nwk::COORDINATOR)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|payload| zdo::decode_active_ep_rsp(&payload).map_err(|e| e.to_string()))
    {
        Ok(response) => {
            outcome.completed.push(InterviewStep::ActiveEndpoints);
            response.endpoints
        }
        Err(detail) => {
            outcome
                .failures
                .push((InterviewStep::ActiveEndpoints, detail));
            Vec::new()
        }
    };
    zigbee
        .interview_update(ieee, InterviewUpdate::Step(InterviewStep::ActiveEndpoints))
        .await?;
    Ok(endpoints)
}

/// Reads one simple descriptor per endpoint.
///
/// Serial on purpose. A sleepy device asked two overlapping questions is a good
/// way to get neither answer, and the interview is already bounded by the
/// per-request timeout.
async fn simple_descriptors(
    zigbee: &Zigbee,
    ieee: Ieee,
    endpoints: Vec<EndpointId>,
    outcome: &mut InterviewOutcome,
) -> Result<(), RuntimeError> {
    for endpoint in endpoints {
        match zigbee
            .zdo(ieee, ZdoClusterId::SIMPLE_DESC_REQ, move |seq| {
                zdo::encode_simple_desc_req(seq, Nwk::COORDINATOR, endpoint)
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|payload| zdo::decode_simple_desc_rsp(&payload).map_err(|e| e.to_string()))
        {
            Ok(descriptor) => {
                outcome.endpoints.push(EndpointInfo {
                    id: descriptor.endpoint,
                    profile: descriptor.profile,
                    device_id: descriptor.device_id,
                    input_clusters: descriptor.input_clusters,
                    output_clusters: descriptor.output_clusters,
                });
                outcome
                    .completed
                    .push(InterviewStep::SimpleDescriptor(endpoint));
            }
            Err(detail) => outcome
                .failures
                .push((InterviewStep::SimpleDescriptor(endpoint), detail)),
        }
        zigbee
            .interview_update(
                ieee,
                InterviewUpdate::Step(InterviewStep::SimpleDescriptor(endpoint)),
            )
            .await?;
    }
    Ok(())
}
