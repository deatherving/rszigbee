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

use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId, Ieee};
use rszigbee_spec::zcl::ZclValue;
use rszigbee_spec::zdo::{self, ZdoClusterId};
use tracing::{debug, warn};

use super::{RuntimeError, Zigbee};
use crate::device::{BasicInfo, DeviceKind, EndpointInfo, InterviewState, PowerSource};
use crate::event::InterviewStep;

/// The `genBasic` cluster.
const GEN_BASIC: ClusterId = ClusterId(0x0000);

/// `genBasic` attribute ids.
mod attr {
    use super::AttrId;

    pub const ZCL_VERSION: AttrId = AttrId(0x0000);
    pub const APP_VERSION: AttrId = AttrId(0x0001);
    pub const STACK_VERSION: AttrId = AttrId(0x0002);
    pub const HW_VERSION: AttrId = AttrId(0x0003);
    pub const MANUFACTURER_NAME: AttrId = AttrId(0x0004);
    pub const MODEL_ID: AttrId = AttrId(0x0005);
    pub const DATE_CODE: AttrId = AttrId(0x0006);
    pub const POWER_SOURCE: AttrId = AttrId(0x0007);
    pub const SW_BUILD_ID: AttrId = AttrId(0x4000);
}

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
    /// What `genBasic` reported, when it answered.
    pub basic: Option<BasicInfo>,
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
        basic: None,
    };

    node_descriptor(zigbee, ieee, &mut outcome).await?;
    let endpoints = active_endpoints(zigbee, ieee, &mut outcome).await?;
    simple_descriptors(zigbee, ieee, endpoints, &mut outcome).await?;
    // Last, because it needs an endpoint to read from, and the endpoint list
    // comes from the step before. Without it there is no model string, and
    // without a model string no definition can be resolved -- so a device that
    // refuses this step is a device that stays unrecognised no matter how good
    // the definition catalogue is.
    basic_attributes(zigbee, ieee, &mut outcome).await?;

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

/// Reads `genBasic`: who made this device and what model it is.
///
/// The model string is the primary key for definition matching, and the
/// manufacturer name is what separates the dozens of unrelated devices sharing
/// a model string like `TS0601`. Everything the compatibility layer does starts
/// here.
///
/// A device that answers only some of these is normal and still useful: a model
/// with no date code resolves perfectly well. Only a missing *model* leaves the
/// device unrecognisable.
async fn basic_attributes(
    zigbee: &Zigbee,
    ieee: Ieee,
    outcome: &mut InterviewOutcome,
) -> Result<(), RuntimeError> {
    // The endpoint that actually hosts genBasic, falling back to the first one
    // the device reported. Reading endpoint 1 unconditionally fails on devices
    // whose application endpoint is numbered differently.
    let endpoint = outcome
        .endpoints
        .iter()
        .find(|e| e.input_clusters.contains(&GEN_BASIC))
        .or_else(|| outcome.endpoints.first())
        .map_or(EndpointId(1), |e| e.id);

    let attributes = vec![
        attr::ZCL_VERSION,
        attr::APP_VERSION,
        attr::STACK_VERSION,
        attr::HW_VERSION,
        attr::MANUFACTURER_NAME,
        attr::MODEL_ID,
        attr::DATE_CODE,
        attr::POWER_SOURCE,
        attr::SW_BUILD_ID,
    ];

    match zigbee.zcl_read(ieee, endpoint, GEN_BASIC, attributes).await {
        Ok(values) => {
            outcome.basic = Some(BasicInfo {
                manufacturer_name: values.iter().find_map(|(id, v)| {
                    (*id == attr::MANUFACTURER_NAME.0)
                        .then(|| text(v))
                        .flatten()
                }),
                model_id: values
                    .iter()
                    .find_map(|(id, v)| (*id == attr::MODEL_ID.0).then(|| text(v)).flatten()),
                date_code: values
                    .iter()
                    .find_map(|(id, v)| (*id == attr::DATE_CODE.0).then(|| text(v)).flatten()),
                software_build_id: values
                    .iter()
                    .find_map(|(id, v)| (*id == attr::SW_BUILD_ID.0).then(|| text(v)).flatten()),
                zcl_version: values
                    .iter()
                    .find_map(|(id, v)| (*id == attr::ZCL_VERSION.0).then(|| small(v)).flatten()),
                app_version: values
                    .iter()
                    .find_map(|(id, v)| (*id == attr::APP_VERSION.0).then(|| small(v)).flatten()),
                stack_version: values
                    .iter()
                    .find_map(|(id, v)| (*id == attr::STACK_VERSION.0).then(|| small(v)).flatten()),
                hardware_version: values
                    .iter()
                    .find_map(|(id, v)| (*id == attr::HW_VERSION.0).then(|| small(v)).flatten()),
            });
            if outcome
                .basic
                .as_ref()
                .and_then(|b| b.model_id.as_ref())
                .is_some()
            {
                outcome.completed.push(InterviewStep::BasicAttributes);
            } else {
                outcome.failures.push((
                    InterviewStep::BasicAttributes,
                    "answered without a modelId, so no definition can be resolved".into(),
                ));
            }
        }
        Err(e) => outcome
            .failures
            .push((InterviewStep::BasicAttributes, e.to_string())),
    }
    zigbee
        .interview_update(ieee, InterviewUpdate::Step(InterviewStep::BasicAttributes))
        .await
}

/// A ZCL string, with control characters stripped.
///
/// Devices pad and terminate strings with NULs and occasionally emit stray
/// control bytes. Left in, they reach logs, filenames and MQTT topics.
fn text(value: &ZclValue) -> Option<String> {
    match value {
        ZclValue::Str(s) => {
            let cleaned: String = s.chars().filter(|c| !c.is_control()).collect();
            let trimmed = cleaned.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        _ => None,
    }
}

/// A small unsigned value, when the device reported one that fits.
fn small(value: &ZclValue) -> Option<u8> {
    match value {
        ZclValue::Uint(v) => u8::try_from(*v).ok(),
        ZclValue::Int(v) => u8::try_from(*v).ok(),
        ZclValue::Enum(v) => u8::try_from(*v).ok(),
        _ => None,
    }
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
        .zdo(ieee, ZdoClusterId::NODE_DESC_REQ, move |seq, nwk| {
            zdo::encode_node_desc_req(seq, nwk)
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
        .zdo(ieee, ZdoClusterId::ACTIVE_EP_REQ, move |seq, nwk| {
            zdo::encode_active_ep_req(seq, nwk)
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
            .zdo(ieee, ZdoClusterId::SIMPLE_DESC_REQ, move |seq, nwk| {
                zdo::encode_simple_desc_req(seq, nwk, endpoint)
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
