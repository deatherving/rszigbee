//! Stack bring-up: endpoint registration, identity, and resuming a stored
//! network.
//!
//! Every step here was added because its absence produced an observed failure
//! on real hardware.
//!
//! # `network_init` is not optional
//!
//! EZSP reports `NoNetwork` from `networkState` until `networkInit` has run,
//! *even when a network is stored in the coordinator's tokens*. Without this
//! call, a coordinator with a perfectly good network looks blank.
//!
//! That is not a cosmetic bug. A stack that reads `NoNetwork` and forms a new
//! network would destroy the existing one and orphan every joined device. The
//! `MismatchPolicy::Fail` default is what turned this into a refusal rather
//! than data loss when it was hit for the first time.
//!
//! EZSP's own documentation is explicit: "This should be called on startup
//! whether the node was previously part of a network", and it returns
//! `NotJoined` when there is nothing stored — which makes it the authoritative
//! answer to "does this coordinator have a network", better than `networkState`.
//!
//! # Endpoints must be registered before the network comes up
//!
//! A coordinator with no application endpoint has nowhere to deliver ZCL
//! traffic to, and `Active_EP_rsp` comes back empty. Observed exactly that.

use ezsp::ezsp::network::InitBitmask;
use ezsp::{Configuration, Networking};
use rszigbee_adapter::AdapterError;
use rszigbee_spec::ids::{ClusterId, EndpointId, ManufacturerCode, ProfileId};
use tracing::{debug, info};

/// The coordinator's primary application endpoint.
pub const PRIMARY_ENDPOINT: EndpointId = EndpointId(1);

/// Device id for the coordinator endpoint. Matches what `Zigbee2MQTT`'s Ember
/// adapter registers, so a device that special-cases the coordinator sees the
/// same value it would from upstream.
const COORDINATOR_DEVICE_ID: u16 = 0x0065;

/// Server-side clusters the coordinator hosts.
///
/// These are the clusters a *device* can talk to on us: `genTime` so devices
/// can read the time, `genOta` so they can ask for firmware, and the basic
/// control clusters.
const IN_CLUSTERS: &[u16] = &[
    0x0000, // genBasic
    0x0003, // genIdentify
    0x0006, // genOnOff
    0x0008, // genLevelCtrl
    0x000a, // genTime
    0x0019, // genOta
    0x0300, // lightingColorCtrl
];

/// Client-side clusters the coordinator sends and receives reports for.
///
/// This list is why it matters: a cluster missing here means **attribute
/// reports for it are never delivered**. A temperature sensor would pair, be
/// interviewed, and then appear silent. Kept aligned with what `Zigbee2MQTT`'s
/// Ember adapter registers, because that list is the product of finding out the
/// hard way which omissions break which devices.
const OUT_CLUSTERS: &[u16] = &[
    0x0000, // genBasic
    0x0003, // genIdentify
    0x0004, // genGroups
    0x0005, // genScenes
    0x0006, // genOnOff
    0x0008, // genLevelCtrl
    0x0020, // genPollCtrl
    0x0300, // lightingColorCtrl
    0x0400, // msIlluminanceMeasurement
    0x0402, // msTemperatureMeasurement
    0x0405, // msRelativeHumidity
    0x0406, // msOccupancySensing
    0x0500, // ssIasZone
    0x0702, // seMetering
    0x0b01, // seMeterIdentification
    0x0b03, // haApplianceStatistics
    0x0b04, // haElectricalMeasurement
    0x1000, // touchlink
];

/// Whether a stored network was found and resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredNetwork {
    /// A network was stored and the stack is now up on it.
    Resumed,
    /// The coordinator has no network stored.
    None,
}

/// Registers the coordinator's endpoints and identity.
///
/// Must run before the network comes up: EZSP rejects `addEndpoint` once the
/// stack is running.
pub async fn configure_endpoints(
    connection: &mut ezsp::Connection,
    manufacturer: ManufacturerCode,
) -> Result<(), AdapterError> {
    connection
        .add_endpoint(
            PRIMARY_ENDPOINT.0,
            ProfileId::HA.0,
            COORDINATOR_DEVICE_ID,
            0,
            IN_CLUSTERS.iter().copied().collect(),
            OUT_CLUSTERS.iter().copied().collect(),
        )
        .await
        .map_err(|e| {
            AdapterError::Transport(format!(
                "cannot register coordinator endpoint {}: {e}. Without an \
                 endpoint the coordinator has nowhere to receive ZCL traffic.",
                PRIMARY_ENDPOINT.0
            ))
        })?;

    debug!(
        endpoint = PRIMARY_ENDPOINT.0,
        in_clusters = IN_CLUSTERS.len(),
        out_clusters = OUT_CLUSTERS.len(),
        "registered coordinator endpoint"
    );

    // Left unset, EZSP reports manufacturer code 0xabcd. That is the exact
    // value zigbee-herdsman's interview quirks use to identify a Control4
    // device, so an unset coordinator misidentifies itself. Observed on real
    // hardware before this call was added.
    connection
        .set_manufacturer_code(manufacturer.0)
        .await
        .map_err(|e| AdapterError::Transport(format!("cannot set the manufacturer code: {e}")))?;

    Ok(())
}

/// Resumes a stored network, if there is one.
pub async fn resume_stored_network(
    connection: &mut ezsp::Connection,
) -> Result<StoredNetwork, AdapterError> {
    // PARENT_INFO_IN_TOKEN preserves an end device's parent across a reboot.
    // Harmless for a coordinator and correct if this adapter is ever used for
    // a non-coordinator role.
    match connection
        .network_init(InitBitmask::PARENT_INFO_IN_TOKEN)
        .await
    {
        Ok(()) => {
            info!("resumed the stored network");
            Ok(StoredNetwork::Resumed)
        }
        Err(e) => {
            // `NotJoined` is the documented answer for "nothing stored". It is a
            // state, not a failure, and conflating the two is what makes a stack
            // form a network over a working one.
            let text = e.to_string();
            if text.contains("NotJoined") || text.contains("NOT_JOINED") {
                debug!("no stored network on this coordinator");
                Ok(StoredNetwork::None)
            } else {
                Err(AdapterError::Transport(format!("network_init failed: {e}")))
            }
        }
    }
}

/// The clusters the coordinator advertises, for diagnostics.
#[must_use]
pub fn advertised_clusters() -> (Vec<ClusterId>, Vec<ClusterId>) {
    (
        IN_CLUSTERS.iter().copied().map(ClusterId).collect(),
        OUT_CLUSTERS.iter().copied().map(ClusterId).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_clusters_cover_the_common_sensor_types() {
        // A cluster missing from the client list means attribute reports for it
        // are silently never delivered: the device pairs, interviews, and then
        // appears dead. These are the ones that would be noticed first.
        for (cluster, what) in [
            (0x0402u16, "temperature"),
            (0x0405, "humidity"),
            (0x0400, "illuminance"),
            (0x0406, "occupancy"),
            (0x0500, "IAS zone"),
            (0x0702, "metering"),
            (0x0b04, "electrical measurement"),
            (0x0006, "on/off"),
            (0x0008, "level"),
        ] {
            assert!(
                OUT_CLUSTERS.contains(&cluster),
                "{what} (0x{cluster:04x}) missing: its reports would never arrive"
            );
        }
    }

    #[test]
    fn poll_control_is_advertised_so_sleepy_devices_can_check_in() {
        // Without genPollCtrl the coordinator cannot learn a device's check-in
        // interval, and buffered commands for sleepy devices never flush.
        assert!(OUT_CLUSTERS.contains(&0x0020));
    }

    #[test]
    fn ota_is_hosted_so_devices_can_request_firmware() {
        assert!(IN_CLUSTERS.contains(&0x0019));
    }

    #[test]
    fn cluster_lists_have_no_duplicates() {
        for list in [IN_CLUSTERS, OUT_CLUSTERS] {
            let mut sorted = list.to_vec();
            let before = sorted.len();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "duplicate cluster in {list:?}");
        }
    }

    #[test]
    fn cluster_lists_fit_in_the_ezsp_byte_sized_vector() {
        // add_endpoint takes ByteSizedVec, capped at 255 entries, whose
        // FromIterator panics on overflow.
        assert!(IN_CLUSTERS.len() <= 255);
        assert!(OUT_CLUSTERS.len() <= 255);
    }

    #[test]
    fn the_primary_endpoint_is_one() {
        // Device definitions and the whole ecosystem assume endpoint 1 is the
        // coordinator's application endpoint.
        assert_eq!(PRIMARY_ENDPOINT, EndpointId(1));
    }

    #[test]
    fn advertised_clusters_are_reported_for_diagnostics() {
        let (input, output) = advertised_clusters();
        assert_eq!(input.len(), IN_CLUSTERS.len());
        assert_eq!(output.len(), OUT_CLUSTERS.len());
        assert!(output.contains(&ClusterId(0x0402)));
    }
}
