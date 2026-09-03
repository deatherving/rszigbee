//! What joining a device should do to it.
//!
//! Materialised as a plan rather than executed here, so an operator can see it
//! before it happens and so it is testable without a radio.
//!
//! The plan is derived from the capabilities as well as from any explicit
//! bindings, and that is the half most easily missed: upstream's
//! `m.temperature()` configures reporting as part of what it means, so a
//! definition transcoded from it has no explicit binding at all. Following only
//! explicit bindings leaves such a device recognised and permanently silent.

use rszigbee_devices::Definition;
use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId};
use rszigbee_spec::zcl::types::ZclType;

use crate::device::DeviceInfo;

use super::sources::sources;

/// One binding-and-reporting step a definition asks for at join time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigureStep {
    /// Endpoint to bind.
    pub endpoint: EndpointId,
    /// Cluster to bind.
    pub cluster: ClusterId,
    /// Attribute to configure reporting for, when there is one.
    pub attribute: Option<AttrId>,
    /// The attribute's wire type.
    ///
    /// Carried on the step rather than looked up later, because the registry
    /// does not know every cluster a definition can name — soil moisture and
    /// CO2 are not in the built-in set — and configuring reporting with the
    /// wrong type produces a frame the device rejects.
    pub attribute_type: Option<ZclType>,
    /// Shortest reporting interval, seconds.
    pub min_interval: u16,
    /// Longest interval before the device reports anyway, seconds.
    pub max_interval: u16,
    /// Smallest change worth reporting.
    pub min_change: u64,
}

/// Materialises the bindings and reporting a definition asks for.
///
/// Producing the plan is separate from executing it on purpose: an operator
/// wants to see what joining a device will do to it before it happens, and a
/// plan that can be inspected is also a plan that can be tested without a
/// radio.
///
/// Without reporting configured a sensor pairs, interviews, and then appears
/// silent forever, which is the most common way a working device looks broken.
#[must_use]
pub fn configure_plan(definition: &Definition, info: &DeviceInfo) -> Vec<ConfigureStep> {
    let mut steps: Vec<ConfigureStep> = Vec::new();
    let mut seen: std::collections::HashSet<(EndpointId, ClusterId, Option<AttrId>)> =
        std::collections::HashSet::new();

    // Explicit bindings first, so that where a definition states an interval
    // its value wins over the default below. The definition knows more about
    // the device than a default does.
    for binding in &definition.bindings {
        // An endpoint the device does not have cannot be bound. Emitting the
        // step anyway would produce a guaranteed failure at join time.
        if !info.endpoints.is_empty() && info.endpoint(binding.endpoint).is_none() {
            continue;
        }
        if binding.reporting.is_empty() {
            steps.push(ConfigureStep {
                endpoint: binding.endpoint,
                cluster: binding.cluster,
                attribute: None,
                attribute_type: None,
                min_interval: 0,
                max_interval: 0,
                min_change: 0,
            });
            continue;
        }
        for reporting in &binding.reporting {
            steps.push(ConfigureStep {
                endpoint: binding.endpoint,
                cluster: binding.cluster,
                attribute: Some(reporting.attribute),
                // An explicit binding does not state a type, so it is resolved
                // from the capability sources when one names the same
                // attribute, and left to the caller otherwise.
                attribute_type: sources(definition)
                    .iter()
                    .find(|s| s.cluster == binding.cluster && s.attribute == reporting.attribute)
                    .map(|s| s.ty),
                min_interval: reporting.min_interval,
                max_interval: reporting.max_interval,
                min_change: reporting.min_change,
            });
        }
    }
    for step in &steps {
        seen.insert((step.endpoint, step.cluster, step.attribute));
    }

    // Then what the capabilities imply. This is the half that matters most:
    // upstream's `m.temperature()` configures reporting as part of what it
    // means, and a definition transcoded from it has an empty `bindings` list.
    // Without this a device joins, interviews, resolves, advertises a
    // temperature capability -- and never reports a temperature, which is
    // indistinguishable from a broken sensor.
    for source in sources(definition) {
        let endpoint = info
            .endpoint_with_input(source.cluster)
            .map_or(EndpointId(1), |e| e.id);
        let key = (endpoint, source.cluster, Some(source.attribute));
        if !seen.insert(key) {
            continue;
        }
        steps.push(ConfigureStep {
            endpoint,
            cluster: source.cluster,
            attribute: Some(source.attribute),
            attribute_type: Some(source.ty),
            min_interval: DEFAULT_MIN_INTERVAL,
            max_interval: DEFAULT_MAX_INTERVAL,
            // Report any change. A threshold suppresses small movements, and
            // choosing one is per-device tuning the definition does not do.
            min_change: 0,
        });
    }
    steps
}

/// Ten seconds. Short enough that a state change is prompt, long enough that a
/// chatty device cannot saturate the network.
const DEFAULT_MIN_INTERVAL: u16 = 10;

/// An hour. This is the number availability depends on: until it elapses, a
/// device that only reports on change is indistinguishable from a dead one.
const DEFAULT_MAX_INTERVAL: u16 = 3600;
