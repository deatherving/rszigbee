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

use rsezsp::Eui64;
use rsezsp::ezsp::command::{
    AddEndpoint, ClearTransientLinkKeys, GetConfigurationValue, ImportTransientKey, NetworkInit,
    SetConfigurationValue, SetManufacturerCode, SetPolicy,
};
use rsezsp::types::network::{ConfigId, Decision, NetworkInitBitmask, PolicyId};
use rsezsp::types::security::{SecurityKey, SecurityManFlags};
use rszigbee_adapter::AdapterError;

use crate::connection::{Connection, check, context};
use rszigbee_spec::ids::{ClusterId, EndpointId, ManufacturerCode, ProfileId};
use tracing::{debug, info, warn};

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
    connection: &Connection,
    manufacturer: ManufacturerCode,
) -> Result<(), AdapterError> {
    let response = connection
        .command(AddEndpoint {
            endpoint: PRIMARY_ENDPOINT.0,
            profile_id: ProfileId::HA.0,
            device_id: COORDINATOR_DEVICE_ID,
            app_flags: 0,
            input_clusters: IN_CLUSTERS.to_vec(),
            output_clusters: OUT_CLUSTERS.to_vec(),
        })
        .await
        .map_err(|e| {
            context(
                &format!(
                    "cannot register coordinator endpoint {}. Without an \
                     endpoint the coordinator has nowhere to receive ZCL traffic",
                    PRIMARY_ENDPOINT.0
                ),
                &e,
            )
        })?;
    check("registering the coordinator endpoint", response.status)?;

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
        .command(SetManufacturerCode {
            code: manufacturer.0,
        })
        .await
        .map_err(|e| context("cannot set the manufacturer code", &e))?;

    Ok(())
}

/// `EmberDecisionBitmask`: admit a joining device that has no link key yet.
const ALLOW_JOIN_BIT: u8 = 0x01;

/// `EmberDecisionBitmask`: admit an unsecured rejoin.
///
/// A device that has lost its parent -- a sleepy end device whose battery was
/// changed, or one that woke outside its poll timeout -- comes back this way.
/// Without the bit it can never return, which reads as a device that paired
/// once and then died.
const ALLOW_UNSECURED_REJOIN_BIT: u8 = 0x02;

/// Sets the stack configuration a coordinator needs before its network is up.
///
/// These are not tuning knobs. [`StackProfile`] and [`SecurityLevel`] are
/// advertised in every beacon the coordinator sends, and
/// [`MaxEndDeviceChildren`] is the end-device capacity a joining device reads
/// out of that beacon. A scanning device that finds the wrong profile, or no
/// capacity for it, **never sends an association request at all** -- so the
/// symptom is silence rather than a refusal, with nothing logged anywhere,
/// which is what makes it expensive to diagnose.
///
/// EZSP refuses these writes once the stack is up, answering `InvalidCall`, so
/// this has to run before `network_init`.
///
/// The previous value is read back and logged before each write. NCP defaults
/// vary by firmware build, and without the old value a failure here cannot be
/// told apart from a default that was already correct.
///
/// [`StackProfile`]: rsezsp::types::network::ConfigId::STACK_PROFILE
/// [`SecurityLevel`]: rsezsp::types::network::ConfigId::SECURITY_LEVEL
/// [`MaxEndDeviceChildren`]: rsezsp::types::network::ConfigId::MAX_END_DEVICE_CHILDREN
pub async fn configure_stack(connection: &Connection) -> Result<(), AdapterError> {
    /// `(id, value, required, what it affects)`.
    ///
    /// `required` marks the three a device reads out of a beacon: getting one
    /// of those wrong makes the coordinator invisible to a joining device, so
    /// failing to set it is worth refusing to start over. The rest are
    /// timeouts and table sizes where a firmware default that differs is a
    /// difference in behaviour, not a broken network.
    const SETTINGS: &[(ConfigId, u16, bool, &str)] = &[
        (
            ConfigId::STACK_PROFILE,
            2,
            true,
            "ZigBee Pro; advertised in every beacon",
        ),
        (
            ConfigId::SECURITY_LEVEL,
            5,
            true,
            "standard security; advertised in every beacon",
        ),
        (
            ConfigId::MAX_END_DEVICE_CHILDREN,
            32,
            true,
            "beacon end-device capacity; sleepy devices join as children",
        ),
        (
            ConfigId::END_DEVICE_POLL_TIMEOUT,
            8,
            false,
            "how long a sleepy child may stay silent before it is dropped",
        ),
        (
            ConfigId::INDIRECT_TRANSMISSION_TIMEOUT,
            7680,
            false,
            "how long a message for a sleepy child is held for its next poll",
        ),
        (
            ConfigId::TRUST_CENTER_ADDRESS_CACHE_SIZE,
            2,
            false,
            "trust-centre address cache",
        ),
    ];

    for &(config_id, value, required, affects) in SETTINGS {
        let before = connection
            .command(GetConfigurationValue { config_id })
            .await
            .ok()
            .filter(|r| r.status.is_ok())
            .map(|r| r.value);

        let outcome = match connection
            .command(SetConfigurationValue { config_id, value })
            .await
        {
            Ok(response) => check("setting a configuration value", response.status),
            Err(e) => Err(context("setting a configuration value", &e)),
        };

        match outcome {
            Ok(()) => debug!(
                ?config_id,
                value,
                ?before,
                affects,
                "stack configuration set"
            ),
            Err(e) if !required => {
                warn!(
                    ?config_id,
                    value, affects, "optional stack configuration refused: {e}"
                );
            }
            Err(e) => {
                return Err(AdapterError::Transport(format!(
                    "cannot set {config_id:?} to {value} ({affects}): {e}. A device \
                     scanning for a network reads this out of the coordinator's \
                     beacon, and will not attempt to join at all if it is wrong."
                )));
            }
        }
    }

    Ok(())
}

/// The well-known Zigbee 3.0 default trust-centre link key.
///
/// Public by design, and specified: `ZigBeeAlliance09` in ASCII. Zigbee 3.0
/// devices that ship without an install code use it to protect the one exchange
/// in which they are given the real network key. It is not a secret and is not
/// treated as one -- the security it provides is that the window in which it is
/// accepted is short and operator-initiated, which is why it is installed when
/// joining opens and removed when joining closes.
const WELL_KNOWN_TC_LINK_KEY: [u8; 16] = *b"ZigBeeAlliance09";

/// Installs the commissioning key for the duration of a join window.
///
/// Without this a Zigbee 3.0 device joins at the MAC layer and then cannot
/// finish commissioning: it has no key with which to receive the network key,
/// so it gives up and rejoins, over and over. The observable symptom is a
/// device that keeps announcing itself every few seconds, never answers a ZDO
/// request, and never stops blinking -- while every join callback looks
/// perfectly healthy.
///
/// Found by differential test: the reference stack imports this key at the
/// moment joining opens (`IMPORT_TRANSIENT_KEY`, frame `0x0111`) and clears it
/// when joining closes. We did neither.
///
/// The key is *transient* deliberately. A joining device is trusted with it
/// only inside the window an operator opened; it is not left in the key table
/// afterwards, which is what [`clear_commissioning_key`] is for.
pub async fn install_commissioning_key(connection: &Connection) -> Result<(), AdapterError> {
    /// Applies to whichever device joins, since which one that will be is not
    /// known until it does. A specific EUI64 here would only admit a device
    /// whose address was known in advance, which is the install-code flow.
    const ANY_DEVICE: Eui64 = Eui64::WILDCARD;

    let response = connection
        .command(ImportTransientKey {
            eui64: ANY_DEVICE,
            key: SecurityKey::new(WELL_KNOWN_TC_LINK_KEY),
            // Below EZSP 14 this field is present and means "no qualifiers on
            // the key". At 14 and above it is not sent at all, which rsezsp
            // handles from the negotiated version.
            flags: SecurityManFlags::NONE,
        })
        .await
        .map_err(|e| {
            context(
                "cannot install the commissioning key. Without it a Zigbee 3.0 \
                 device joins at the MAC layer and can never finish commissioning",
                &e,
            )
        })?;
    check("installing the commissioning key", response.status)?;

    debug!("commissioning key installed for the join window");
    Ok(())
}

/// Removes the commissioning key again once joining is closed.
///
/// The counterpart to [`install_commissioning_key`]. Leaving the well-known key
/// installed would let a device commission against a key everyone knows at any
/// later moment, rather than only inside a window an operator opened.
pub async fn clear_commissioning_key(connection: &Connection) -> Result<(), AdapterError> {
    connection
        .command(ClearTransientLinkKeys)
        .await
        .map_err(|e| context("cannot clear the commissioning key", &e))?;
    debug!("commissioning key cleared");
    Ok(())
}

/// Sets the trust-centre policies a coordinator needs to admit a device.
///
/// `permitJoining` only opens the MAC association window. Whether a device is
/// actually admitted, and whether it is given the network key, is a separate
/// *trust-centre* decision — and `EmberZNet`'s default for that is to allow only
/// devices whose key was preconfigured, which no ordinary device has. So
/// joining appeared to work and nothing ever joined: the window opened, the
/// device tried, and the trust centre silently declined.
///
/// Observed exactly that against real firmware, with a device in pairing mode
/// and a full 60-second window producing no events at all.
///
/// The two that matter:
///
/// * **Trust centre**: allow joins and rejoins. Without it nothing can join at
///   all.
/// * **TC key request**: Zigbee 3.0 devices ask the trust centre for the link
///   key as part of joining, and a device whose request is denied drops off
///   again shortly after appearing to succeed.
///
/// Application key requests stay denied. That is key material for
/// device-to-device encryption, nothing here needs it, and granting it widens
/// what a joined device can ask for on the basis that it happened to ask.
pub async fn configure_join_policies(connection: &Connection) -> Result<(), AdapterError> {
    /// `ALLOW_JOINS | ALLOW_UNSECURED_REJOINS`, as a bitmask.
    ///
    /// Deliberately a literal rather than `decision::Id::AllowJoins`, which is
    /// `0x00`. That name belongs to the pre-EZSP-8 `EzspDecisionId` enum, in
    /// which zero meant "send the network key in the clear to every joiner".
    /// `EmberZNet` 7.x reinterprets the same field as `EmberDecisionBitmask`,
    /// where zero is `DEFAULT_CONFIGURATION` -- deny every join. So the enum
    /// sets the policy to *deny* under a name that reads as *allow*, and the
    /// log line below then reports joining as enabled while nothing can join.
    ///
    /// Found by differential test against a reference stack on `EmberZNet`
    /// 7.4.4: a device in pairing mode produced no callback of any kind across a
    /// full 240-second window, while the reference admitted the same device in
    /// about thirty seconds with this field set to 3.
    const ALLOW_JOINS: u8 = ALLOW_JOIN_BIT | ALLOW_UNSECURED_REJOIN_BIT;
    /// Answer a joining device's key request with the current link key.
    const SEND_CURRENT_KEY: u8 = Decision::ALLOW_TC_KEY_REQUEST_SAME_KEY.0;
    /// Refuse application link key requests.
    const DENY_APP_KEYS: u8 = Decision::DENY_APP_KEY_REQUESTS.0;

    for (policy_id, decision, what) in [
        (PolicyId::TRUST_CENTER, ALLOW_JOINS, "admit joining devices"),
        (
            PolicyId::TC_KEY_REQUEST,
            SEND_CURRENT_KEY,
            "answer link key requests",
        ),
        (
            PolicyId::APP_KEY_REQUEST,
            DENY_APP_KEYS,
            "refuse application key requests",
        ),
    ] {
        let response = connection
            .command(SetPolicy {
                policy_id,
                decision: Decision(decision),
            })
            .await
            .map_err(|e| {
                context(
                    &format!(
                        "cannot set the {policy_id:?} policy to {what}. Without it a \
                         device cannot join even while joining is open"
                    ),
                    &e,
                )
            })?;
        check(
            &format!("setting the {policy_id:?} policy"),
            response.status,
        )?;
    }

    debug!("trust-centre policies set: joins allowed, link keys answered");
    Ok(())
}

/// Resumes a stored network, if there is one.
pub async fn resume_stored_network(connection: &Connection) -> Result<StoredNetwork, AdapterError> {
    /// `EMBER_NOT_JOINED`, the documented answer for "nothing stored".
    ///
    /// Matched on the value rather than on the text of an error message. The
    /// previous implementation searched the formatted error for `NotJoined`,
    /// which silently depends on an upstream `Display` impl -- a rename there
    /// would turn "no network yet" into a hard failure, or worse, turn a real
    /// failure into "no network" and form a network over a working one.
    const NOT_JOINED: u32 = 0x93;

    // PARENT_INFO_IN_TOKEN preserves an end device's parent across a reboot.
    // Harmless for a coordinator and correct if this adapter is ever used for
    // a non-coordinator role.
    let response = connection
        .command(NetworkInit {
            bitmask: NetworkInitBitmask::PARENT_INFO_IN_TOKEN,
        })
        .await
        .map_err(|e| context("network_init failed", &e))?;

    if response.status.is_ok() {
        info!("resumed the stored network");
        return Ok(StoredNetwork::Resumed);
    }
    if response.status.0 == NOT_JOINED {
        debug!("no stored network on this coordinator");
        return Ok(StoredNetwork::None);
    }
    Err(AdapterError::Transport(format!(
        "network_init failed: {}",
        response.status
    )))
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
