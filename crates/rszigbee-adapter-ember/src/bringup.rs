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
use ezsp::ember::Eui64;
use ezsp::{Configuration, Networking, Security};
use rszigbee_adapter::AdapterError;
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
/// [`StackProfile`]: ezsp::ezsp::config::Id::StackProfile
/// [`SecurityLevel`]: ezsp::ezsp::config::Id::SecurityLevel
/// [`MaxEndDeviceChildren`]: ezsp::ezsp::config::Id::MaxEndDeviceChildren
pub async fn configure_stack(connection: &mut ezsp::Connection) -> Result<(), AdapterError> {
    use ezsp::ezsp::config;

    /// `(id, value, required, what it affects)`.
    ///
    /// `required` marks the three a device reads out of a beacon: getting one
    /// of those wrong makes the coordinator invisible to a joining device, so
    /// failing to set it is worth refusing to start over. The rest are
    /// timeouts and table sizes where a firmware default that differs is a
    /// difference in behaviour, not a broken network.
    const SETTINGS: &[(config::Id, u16, bool, &str)] = &[
        (
            config::Id::StackProfile,
            2,
            true,
            "ZigBee Pro; advertised in every beacon",
        ),
        (
            config::Id::SecurityLevel,
            5,
            true,
            "standard security; advertised in every beacon",
        ),
        (
            config::Id::MaxEndDeviceChildren,
            32,
            true,
            "beacon end-device capacity; sleepy devices join as children",
        ),
        (
            config::Id::EndDevicePollTimeout,
            8,
            false,
            "how long a sleepy child may stay silent before it is dropped",
        ),
        (
            config::Id::IndirectTransmissionTimeout,
            7680,
            false,
            "how long a message for a sleepy child is held for its next poll",
        ),
        (
            config::Id::TrustCenterAddressCacheSize,
            2,
            false,
            "trust-centre address cache",
        ),
    ];

    for &(id, value, required, affects) in SETTINGS {
        let before = connection.get_configuration_value(id).await.ok();
        match connection.set_configuration_value(id, value).await {
            Ok(()) => debug!(?id, value, ?before, affects, "stack configuration set"),
            Err(e) if !required => {
                warn!(?id, value, affects, "optional stack configuration refused: {e}");
            }
            Err(e) => {
                return Err(AdapterError::Transport(format!(
                    "cannot set {id:?} to {value} ({affects}): {e}. A device \
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
pub async fn install_commissioning_key(
    connection: &mut ezsp::Connection,
) -> Result<(), AdapterError> {
    use silizium::zigbee::security::man::{Context, DerivedKeyType, Flags, KeyType};

    /// Applies to whichever device joins, since which one that will be is not
    /// known until it does. A specific EUI64 here would only admit a device
    /// whose address was known in advance, which is the install-code flow.
    const ANY_DEVICE: [u8; 8] = [0xff; 8];

    let context = Context::new(
        // A trust-centre link key with a timeout: the NCP ages it out on its
        // own, so a window that is never explicitly closed does not leave the
        // key accepted indefinitely.
        KeyType::TcLinkWithTimeout,
        0,
        DerivedKeyType::None,
        Eui64::from(ANY_DEVICE),
        0,
        Flags::NONE,
        0,
    );

    connection
        .import_transient_key(
            context,
            Eui64::from(ANY_DEVICE),
            WELL_KNOWN_TC_LINK_KEY,
            Flags::NONE,
        )
        .await
        .map_err(|e| {
            AdapterError::Transport(format!(
                "cannot install the commissioning key: {e}. Without it a Zigbee \
                 3.0 device can join but cannot finish commissioning, and will \
                 rejoin indefinitely."
            ))
        })?;

    debug!("commissioning key installed for this join window");
    Ok(())
}

/// Removes the commissioning key again once joining is closed.
///
/// The counterpart to [`install_commissioning_key`]. Leaving the well-known key
/// installed would let a device commission against a key everyone knows at any
/// later moment, rather than only inside a window an operator opened.
pub async fn clear_commissioning_key(
    connection: &mut ezsp::Connection,
) -> Result<(), AdapterError> {
    connection.clear_transient_link_keys().await.map_err(|e| {
        AdapterError::Transport(format!("cannot clear the commissioning key: {e}"))
    })?;
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
pub async fn configure_join_policies(
    connection: &mut ezsp::Connection,
) -> Result<(), AdapterError> {
    use ezsp::ezsp::{decision, policy};

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
    const SEND_CURRENT_KEY: u8 = decision::Id::AllowTcKeyRequestsAndSendCurrentKey as u8;
    /// Refuse application link key requests.
    const DENY_APP_KEYS: u8 = decision::Id::DenyAppKeyRequests as u8;

    for (id, decision, what) in [
        (
            policy::Id::TrustCenter,
            ALLOW_JOINS,
            "admit joining devices",
        ),
        (
            policy::Id::TcKeyRequest,
            SEND_CURRENT_KEY,
            "answer link key requests",
        ),
        (
            policy::Id::AppKeyRequest,
            DENY_APP_KEYS,
            "refuse application key requests",
        ),
    ] {
        connection
            .set_policy(id, decision)
            .await
            .map_err(|e| {
                AdapterError::Transport(format!(
                    "cannot set the {id:?} policy to {what}: {e}. Without it a \
                     device cannot join even while joining is open."
                ))
            })?;
    }

    debug!("trust-centre policies set: joins allowed, link keys answered");
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
