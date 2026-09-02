//! The device and endpoint model.

use std::time::{Duration, SystemTime};

use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee, Nwk, ProfileId};

/// What kind of node this is.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceKind {
    /// The coordinator itself.
    Coordinator,
    /// A mains-powered router.
    Router,
    /// An end device, usually battery powered.
    EndDevice,
    /// A Green Power device.
    GreenPower,
    /// Not yet determined — the node descriptor has not been read.
    #[default]
    Unknown,
}

/// How the device is powered, as it reports in `genBasic.powerSource`.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerSource {
    /// Mains, any phase configuration.
    Mains,
    /// Battery.
    Battery,
    /// DC source.
    Dc,
    /// Emergency mains.
    EmergencyMains,
    /// Not reported. Distinct from `Battery`: guessing "battery" for an unknown
    /// power source would suppress probing for mains devices that simply did
    /// not answer that read during the interview.
    #[default]
    Unknown,
}

/// How far the interview got.
///
/// `InProgress` is never persisted — it is written as `Pending` so that a crash
/// mid-interview resumes rather than leaving a device stuck forever. Upstream
/// does the same thing, and it is worth copying.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterviewState {
    /// Not started, or resumed after an interrupted attempt.
    #[default]
    Pending,
    /// Running now.
    InProgress,
    /// Completed.
    Successful,
    /// Failed. The device may still be usable: a failed interview often still
    /// learned the endpoints and clusters, and upstream's quirk table exists
    /// precisely because some devices never complete a clean interview.
    Failed,
}

impl InterviewState {
    /// The value to persist for this state.
    #[must_use]
    pub const fn to_persisted(self) -> Self {
        match self {
            Self::InProgress => Self::Pending,
            other => other,
        }
    }

    /// True once the interview has stopped, successfully or not.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Successful | Self::Failed)
    }
}

/// What `genBasic` told us.
#[cfg_attr(feature = "file-store", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BasicInfo {
    /// `manufacturerName`, with control characters stripped.
    pub manufacturer_name: Option<String>,
    /// `modelId`, with control characters stripped. The primary key for
    /// definition matching.
    pub model_id: Option<String>,
    /// `dateCode`.
    pub date_code: Option<String>,
    /// `swBuildId`.
    pub software_build_id: Option<String>,
    /// `zclVersion`.
    pub zcl_version: Option<u8>,
    /// `appVersion`.
    pub app_version: Option<u8>,
    /// `stackVersion`.
    pub stack_version: Option<u8>,
    /// `hwVersion`.
    pub hardware_version: Option<u8>,
}

/// One endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInfo {
    /// Endpoint number.
    pub id: EndpointId,
    /// Application profile.
    pub profile: ProfileId,
    /// Device id within the profile.
    pub device_id: u16,
    /// Server-side clusters this endpoint hosts.
    pub input_clusters: Vec<ClusterId>,
    /// Client-side clusters this endpoint sends to.
    pub output_clusters: Vec<ClusterId>,
}

impl EndpointInfo {
    /// True when this endpoint hosts the cluster as a server.
    #[must_use]
    pub fn has_input(&self, cluster: ClusterId) -> bool {
        self.input_clusters.contains(&cluster)
    }

    /// True when this endpoint sends the cluster as a client.
    #[must_use]
    pub fn has_output(&self, cluster: ClusterId) -> bool {
        self.output_clusters.contains(&cluster)
    }
}

/// A snapshot of one device.
///
/// A plain value with no interior mutability and no reference back to the
/// runtime, so it is safe to hold, log, compare or send between tasks. Callers
/// that need live data ask again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Permanent address.
    pub ieee: Ieee,
    /// Current short address.
    pub nwk: Nwk,
    /// Node type.
    pub kind: DeviceKind,
    /// Power source.
    pub power_source: PowerSource,
    /// Interview progress.
    pub interview: InterviewState,
    /// What `genBasic` reported.
    pub basic: BasicInfo,
    /// Endpoints, in ascending id order.
    pub endpoints: Vec<EndpointInfo>,
    /// The `genPollCtrl` check-in interval, when the device has one. This is
    /// what makes buffering commands for a sleepy device possible.
    pub checkin_interval: Option<Duration>,
    /// Last received frame.
    pub last_seen: Option<SystemTime>,
    /// Last reported link quality.
    pub link_quality: Option<u8>,
}

impl DeviceInfo {
    /// A device known only by its addresses, before any interview.
    #[must_use]
    pub fn new(ieee: Ieee, nwk: Nwk, kind: DeviceKind) -> Self {
        Self {
            ieee,
            nwk,
            kind,
            power_source: PowerSource::Unknown,
            interview: InterviewState::Pending,
            basic: BasicInfo::default(),
            endpoints: Vec::new(),
            checkin_interval: None,
            last_seen: None,
            link_quality: None,
        }
    }

    /// An endpoint by number.
    #[must_use]
    pub fn endpoint(&self, id: EndpointId) -> Option<&EndpointInfo> {
        self.endpoints.iter().find(|e| e.id == id)
    }

    /// The first endpoint hosting `cluster` as a server.
    #[must_use]
    pub fn endpoint_with_input(&self, cluster: ClusterId) -> Option<&EndpointInfo> {
        self.endpoints.iter().find(|e| e.has_input(cluster))
    }

    /// Whether this device is expected to sleep.
    ///
    /// The classification drives whether probing it makes sense. It mirrors
    /// upstream's rule: a router on mains is active, an explicitly non-battery
    /// known power source is active, and **everything else — including an
    /// unknown power source — is treated as sleepy**, because being wrong in
    /// that direction merely delays an offline notification, while being wrong
    /// the other way spams a battery device with pings.
    #[must_use]
    pub fn is_sleepy(&self) -> bool {
        // genPollCtrl is definitive: the device told us it sleeps.
        if self.checkin_interval.is_some() {
            return true;
        }
        match self.power_source {
            PowerSource::Battery => true,
            PowerSource::Mains | PowerSource::Dc | PowerSource::EmergencyMains => false,
            // Power source unreported: trust the node type only for the
            // coordinator, which is definitionally awake.
            PowerSource::Unknown => !matches!(self.kind, DeviceKind::Coordinator),
        }
    }

    /// A short human label for logs and diagnostics.
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.basic.manufacturer_name, &self.basic.model_id) {
            (Some(m), Some(mo)) => format!("{m} {mo} ({})", self.ieee),
            (None, Some(mo)) => format!("{mo} ({})", self.ieee),
            _ => format!("unknown device ({})", self.ieee),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(kind: DeviceKind, power: PowerSource) -> DeviceInfo {
        let mut d = DeviceInfo::new(Ieee::new(1), Nwk::new(2), kind);
        d.power_source = power;
        d
    }

    #[test]
    fn interview_in_progress_persists_as_pending() {
        // A crash mid-interview must resume, not leave a device wedged in a
        // state nothing will ever move it out of.
        assert_eq!(
            InterviewState::InProgress.to_persisted(),
            InterviewState::Pending
        );
        assert_eq!(
            InterviewState::Successful.to_persisted(),
            InterviewState::Successful
        );
        assert_eq!(
            InterviewState::Failed.to_persisted(),
            InterviewState::Failed
        );
        assert_eq!(
            InterviewState::Pending.to_persisted(),
            InterviewState::Pending
        );
    }

    #[test]
    fn a_failed_interview_is_settled_so_the_device_is_still_usable() {
        assert!(InterviewState::Failed.is_settled());
        assert!(InterviewState::Successful.is_settled());
        assert!(!InterviewState::Pending.is_settled());
        assert!(!InterviewState::InProgress.is_settled());
    }

    #[test]
    fn mains_routers_are_not_sleepy() {
        assert!(!dev(DeviceKind::Router, PowerSource::Mains).is_sleepy());
        assert!(!dev(DeviceKind::Router, PowerSource::Dc).is_sleepy());
        assert!(!dev(DeviceKind::Coordinator, PowerSource::Mains).is_sleepy());
    }

    #[test]
    fn battery_devices_are_sleepy_whatever_their_type_claims() {
        // Some battery devices report themselves as routers. Believing the type
        // over the power source would get them pinged flat.
        assert!(dev(DeviceKind::EndDevice, PowerSource::Battery).is_sleepy());
        assert!(dev(DeviceKind::Router, PowerSource::Battery).is_sleepy());
    }

    #[test]
    fn an_unknown_power_source_is_treated_as_sleepy() {
        // Being wrong this way delays an offline notification. Being wrong the
        // other way floods a battery device with probes.
        assert!(dev(DeviceKind::EndDevice, PowerSource::Unknown).is_sleepy());
        assert!(dev(DeviceKind::Unknown, PowerSource::Unknown).is_sleepy());
    }

    #[test]
    fn a_checkin_interval_makes_a_device_sleepy_regardless() {
        let mut d = dev(DeviceKind::Router, PowerSource::Mains);
        assert!(!d.is_sleepy());
        d.checkin_interval = Some(Duration::from_secs(3600));
        assert!(d.is_sleepy(), "genPollCtrl implies the device sleeps");
    }

    #[test]
    fn endpoints_are_searchable_by_cluster() {
        let mut d = dev(DeviceKind::Router, PowerSource::Mains);
        d.endpoints.push(EndpointInfo {
            id: EndpointId(1),
            profile: ProfileId::HA,
            device_id: 0x0100,
            input_clusters: vec![ClusterId(0x0006), ClusterId(0x0008)],
            output_clusters: vec![],
        });
        d.endpoints.push(EndpointInfo {
            id: EndpointId(2),
            profile: ProfileId::HA,
            device_id: 0x0100,
            input_clusters: vec![ClusterId(0x0402)],
            output_clusters: vec![],
        });

        assert_eq!(d.endpoint(EndpointId(2)).map(|e| e.id), Some(EndpointId(2)));
        assert!(d.endpoint(EndpointId(9)).is_none());
        assert_eq!(
            d.endpoint_with_input(ClusterId(0x0402)).map(|e| e.id),
            Some(EndpointId(2))
        );
        assert!(d.endpoint_with_input(ClusterId(0x0300)).is_none());
    }

    #[test]
    fn labels_degrade_gracefully_for_unidentified_devices() {
        let mut d = dev(DeviceKind::Unknown, PowerSource::Unknown);
        assert!(d.label().starts_with("unknown device (0x"));
        d.basic.model_id = Some("TS0601".into());
        assert!(d.label().starts_with("TS0601 (0x"));
        d.basic.manufacturer_name = Some("_TZE200_x".into());
        assert!(d.label().starts_with("_TZE200_x TS0601 (0x"));
    }
}
