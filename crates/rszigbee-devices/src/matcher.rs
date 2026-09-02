//! Resolving which definition describes a device.
//!
//! # This deliberately reproduces upstream's semantics
//!
//! The rules below are a clean-room reimplementation of the resolution order in
//! zigbee-herdsman-converters `src/index.ts` (MIT), studied for behaviour and
//! reimplemented rather than translated. Line references are to 26.104.0.
//!
//! Matching upstream exactly matters more than matching it elegantly: a device
//! that resolves here to a different definition than it does upstream is a
//! device that behaves differently for no reason its owner can see, and the
//! whole point of transcoding upstream's data is that the behaviour comes with
//! it.
//!
//! Three details are easy to get wrong and are each covered by a test:
//!
//! 1. **A sole candidate with a model list short-circuits.** If the index
//!    yields exactly one candidate and it has a model list, upstream returns it
//!    without consulting fingerprints at all (`index.ts:545`). A stricter
//!    implementation that always ran the fingerprint pass would *fail* to match
//!    devices upstream matches.
//! 2. **Fingerprint priority breaks ties by first-wins.** Upstream keeps a
//!    candidate only when its priority is *strictly greater* than the best so
//!    far (`index.ts:559`), so among equal priorities the earliest wins.
//! 3. **Cluster and endpoint lists compare as sets, not sequences.** Upstream's
//!    `arrayEquals` checks equal length and containment (`index.ts:87`), so
//!    order does not matter. Comparing sequences would reject devices that
//!    report their clusters in a different order.
//!
//! Two fields are not plain equality: `ieee` is a pattern, and `endpoints`
//! compares the endpoint id set and then each named endpoint's contents.

use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee, ProfileId};

/// The device facts a match is decided on.
///
/// Built from whatever the interview learned. Every field is optional because
/// an interview can be partial and a definition is often still resolvable — a
/// device that answered `genBasic` but refused its endpoints can still match on
/// model and manufacturer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeviceMatch {
    /// `genBasic.modelId`, the primary key.
    pub model_id: Option<String>,
    /// `genBasic.manufacturerName`.
    pub manufacturer_name: Option<String>,
    /// The manufacturer code from the node descriptor.
    pub manufacturer_id: Option<u16>,
    /// `genBasic.appVersion`.
    pub application_version: Option<u8>,
    /// `genBasic.stackVersion`.
    pub stack_version: Option<u8>,
    /// `genBasic.zclVersion`.
    pub zcl_version: Option<u8>,
    /// `genBasic.hwVersion`.
    pub hardware_version: Option<u8>,
    /// `genBasic.dateCode`.
    pub date_code: Option<String>,
    /// `genBasic.swBuildId`.
    pub software_build_id: Option<String>,
    /// `genBasic.powerSource`, as the raw enum value.
    pub power_source: Option<u8>,
    /// The node's logical type, as the raw value.
    pub device_type: Option<u8>,
    /// The permanent address. Matched as a pattern, not for equality.
    pub ieee: Option<Ieee>,
    /// Endpoints, as the interview found them.
    pub endpoints: Vec<EndpointMatch>,
}

/// One endpoint, for matching.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EndpointMatch {
    /// Endpoint number.
    pub id: EndpointId,
    /// Application profile.
    pub profile: Option<ProfileId>,
    /// Device id within the profile.
    pub device_id: Option<u16>,
    /// Server-side clusters.
    pub input_clusters: Vec<ClusterId>,
    /// Client-side clusters.
    pub output_clusters: Vec<ClusterId>,
}

/// How a definition claims a device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct MatchRules {
    /// Model strings this definition claims outright.
    ///
    /// The common case: 77% of upstream definitions match this way alone.
    pub models: Vec<String>,
    /// Narrower rules, tried before `models`.
    ///
    /// Needed because 21% of upstream definitions match *only* this way: many
    /// vendors ship different hardware under one model string, and the only
    /// thing separating them is the manufacturer name, an endpoint layout, or a
    /// firmware version.
    pub fingerprints: Vec<Fingerprint>,
}

/// A narrow match rule. Every populated field must match.
///
/// An empty fingerprint matches everything, which is why
/// [`Fingerprint::is_empty`] exists and why the index refuses one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct Fingerprint {
    /// Required model string.
    pub model_id: Option<String>,
    /// Required manufacturer name.
    pub manufacturer_name: Option<String>,
    /// Required manufacturer code.
    pub manufacturer_id: Option<u16>,
    /// Required application version.
    pub application_version: Option<u8>,
    /// Required stack version.
    pub stack_version: Option<u8>,
    /// Required ZCL version.
    pub zcl_version: Option<u8>,
    /// Required hardware version.
    pub hardware_version: Option<u8>,
    /// Required date code.
    pub date_code: Option<String>,
    /// Required software build id.
    pub software_build_id: Option<String>,
    /// Required power source.
    pub power_source: Option<u8>,
    /// Required logical type.
    pub device_type: Option<u8>,
    /// A prefix the address must start with.
    ///
    /// Upstream treats this as a regular expression. All 18 patterns in
    /// 26.104.0 are of the form `^<hex prefix><dots>$` where the prefix plus
    /// the wildcard tail is exactly 18 characters — the width of a canonical
    /// `0x` + 16 hex address — so against a canonically rendered address each
    /// one is *equivalent* to a prefix test. That equivalence is the reason a
    /// prefix is enough here, not an assumption that the patterns are literal:
    /// they are not.
    ///
    /// The equivalence is a property of the current patterns, so the transcoder
    /// must verify it per pattern rather than trust it, and report any pattern
    /// that needs a real regex as [`Extend::Unsupported`] instead of silently
    /// approximating it. Approximating would claim devices this definition was
    /// never written for.
    ///
    /// [`Extend::Unsupported`]: crate::Extend::Unsupported
    pub ieee_prefix: Option<String>,
    /// Required endpoint layout.
    pub endpoints: Vec<FingerprintEndpoint>,
    /// Tie-break among fingerprints that all match. Higher wins; equal
    /// priorities resolve first-wins, as upstream does.
    pub priority: i32,
}

/// One endpoint inside a [`Fingerprint`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct FingerprintEndpoint {
    /// Endpoint number, which must be present on the device.
    pub id: EndpointId,
    /// Required profile.
    pub profile: Option<ProfileId>,
    /// Required device id.
    pub device_id: Option<u16>,
    /// Required server clusters, compared as a set.
    pub input_clusters: Option<Vec<ClusterId>>,
    /// Required client clusters, compared as a set.
    pub output_clusters: Option<Vec<ClusterId>>,
}

/// Endpoint 1, not 0: endpoint 0 is ZDO, so defaulting to it would name the
/// one endpoint that never hosts an application cluster.
const PRIMARY: EndpointId = EndpointId(1);

impl DeviceMatch {
    /// A match input for a device that reported this model and nothing else.
    ///
    /// These types are `#[non_exhaustive]` so that learning to match on a new
    /// device fact is not a breaking change. That also means a caller in
    /// another crate cannot write a struct literal, so the constructors here
    /// are not a convenience — without them the type is unusable outside this
    /// crate, including by the runtime that has to build one.
    #[must_use]
    pub fn for_model(model_id: impl Into<String>) -> Self {
        Self {
            model_id: Some(model_id.into()),
            ..Self::default()
        }
    }

    /// Adds the manufacturer name, the field that separates most devices
    /// sharing a model string.
    #[must_use]
    pub fn with_manufacturer(mut self, name: impl Into<String>) -> Self {
        self.manufacturer_name = Some(name.into());
        self
    }
}

impl EndpointMatch {
    /// An endpoint with nothing known but its number.
    #[must_use]
    pub fn new(id: EndpointId) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }
}

impl FingerprintEndpoint {
    /// A requirement that an endpoint with this number exists.
    #[must_use]
    pub fn new(id: EndpointId) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }
}

impl Default for EndpointMatch {
    fn default() -> Self {
        Self {
            id: PRIMARY,
            profile: None,
            device_id: None,
            input_clusters: Vec::new(),
            output_clusters: Vec::new(),
        }
    }
}

impl Default for FingerprintEndpoint {
    fn default() -> Self {
        Self {
            id: PRIMARY,
            profile: None,
            device_id: None,
            input_clusters: None,
            output_clusters: None,
        }
    }
}

impl Fingerprint {
    /// Whether this fingerprint constrains nothing, and so matches every
    /// device.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model_id.is_none()
            && self.manufacturer_name.is_none()
            && self.manufacturer_id.is_none()
            && self.application_version.is_none()
            && self.stack_version.is_none()
            && self.zcl_version.is_none()
            && self.hardware_version.is_none()
            && self.date_code.is_none()
            && self.software_build_id.is_none()
            && self.power_source.is_none()
            && self.device_type.is_none()
            && self.ieee_prefix.is_none()
            && self.endpoints.is_empty()
    }

    /// The address prefix this fingerprint requires, if any.
    #[must_use]
    pub fn ieee_prefix(&self) -> Option<&str> {
        self.ieee_prefix.as_deref()
    }

    /// Whether `device` satisfies every field this fingerprint populates.
    #[must_use]
    pub fn matches(&self, device: &DeviceMatch) -> bool {
        fn same<T: PartialEq>(required: Option<&T>, found: Option<&T>) -> bool {
            match required {
                None => true,
                // A requirement the interview never learned is not a match.
                // Treating "unknown" as "matches" is how a device gets claimed
                // by a definition meant for a different one.
                Some(want) => found == Some(want),
            }
        }

        if !same(self.model_id.as_ref(), device.model_id.as_ref())
            || !same(
                self.manufacturer_name.as_ref(),
                device.manufacturer_name.as_ref(),
            )
            || !same(
                self.manufacturer_id.as_ref(),
                device.manufacturer_id.as_ref(),
            )
            || !same(
                self.application_version.as_ref(),
                device.application_version.as_ref(),
            )
            || !same(self.stack_version.as_ref(), device.stack_version.as_ref())
            || !same(self.zcl_version.as_ref(), device.zcl_version.as_ref())
            || !same(
                self.hardware_version.as_ref(),
                device.hardware_version.as_ref(),
            )
            || !same(self.date_code.as_ref(), device.date_code.as_ref())
            || !same(
                self.software_build_id.as_ref(),
                device.software_build_id.as_ref(),
            )
            || !same(self.power_source.as_ref(), device.power_source.as_ref())
            || !same(self.device_type.as_ref(), device.device_type.as_ref())
        {
            return false;
        }

        if let Some(prefix) = &self.ieee_prefix {
            let Some(ieee) = device.ieee else {
                return false;
            };
            if !ieee.to_string().starts_with(prefix) {
                return false;
            }
        }

        self.endpoints_match(device)
    }

    fn endpoints_match(&self, device: &DeviceMatch) -> bool {
        if self.endpoints.is_empty() {
            return true;
        }

        // Upstream requires the endpoint *id sets* to be equal, not merely that
        // the required ones are present: a device with an extra endpoint is a
        // different device. Length plus containment, so order is irrelevant.
        if self.endpoints.len() != device.endpoints.len() {
            return false;
        }
        for required in &self.endpoints {
            if !device.endpoints.iter().any(|e| e.id == required.id) {
                return false;
            }
        }

        for required in &self.endpoints {
            let Some(found) = device.endpoints.iter().find(|e| e.id == required.id) else {
                return false;
            };
            if let Some(profile) = required.profile
                && found.profile != Some(profile)
            {
                return false;
            }
            if let Some(device_id) = required.device_id
                && found.device_id != Some(device_id)
            {
                return false;
            }
            if let Some(wanted) = &required.input_clusters
                && !set_equal(wanted, &found.input_clusters)
            {
                return false;
            }
            if let Some(wanted) = &required.output_clusters
                && !set_equal(wanted, &found.output_clusters)
            {
                return false;
            }
        }
        true
    }
}

/// Equal length and mutual containment, matching upstream's `arrayEquals`.
///
/// Not sequence equality: devices report cluster lists in whatever order their
/// firmware happens to use, and comparing order would reject devices upstream
/// accepts.
fn set_equal<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

/// Strips a model string down to the form used for indexing.
///
/// Devices routinely report a NUL-padded fixed-width string, so `"TS0601\0\0"`
/// and `"TS0601"` are the same model. Upstream normalises by cutting at the
/// first NUL and trimming (`index.ts:139`); anything else means the same device
/// matching or not depending on how its firmware pads a buffer.
#[must_use]
pub fn normalise_model(model: &str) -> String {
    let cut = model.split('\0').next().unwrap_or(model);
    cut.trim().to_owned()
}

/// The key a model is indexed under: normalised and lowercased.
#[must_use]
pub fn index_key(model: &str) -> String {
    normalise_model(model).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceMatch {
        DeviceMatch {
            model_id: Some("TS0601".into()),
            manufacturer_name: Some("_TZE200_abcdefgh".into()),
            ..DeviceMatch::default()
        }
    }

    #[test]
    fn an_empty_fingerprint_constrains_nothing_and_says_so() {
        // The index refuses these, because one would claim every device.
        assert!(Fingerprint::default().is_empty());
        assert!(Fingerprint::default().matches(&device()));
    }

    #[test]
    fn every_populated_field_must_match() {
        let fp = Fingerprint {
            model_id: Some("TS0601".into()),
            manufacturer_name: Some("_TZE200_abcdefgh".into()),
            ..Fingerprint::default()
        };
        assert!(fp.matches(&device()));

        let other = Fingerprint {
            manufacturer_name: Some("_TZE200_different".into()),
            ..fp.clone()
        };
        // Same model, different manufacturer: this is the case fingerprints
        // exist for. Tuya ships dozens of unrelated devices as "TS0601".
        assert!(!other.matches(&device()));
    }

    #[test]
    fn a_requirement_the_interview_never_learned_does_not_match() {
        let fp = Fingerprint {
            application_version: Some(70),
            ..Fingerprint::default()
        };
        // The device did not report an application version. Treating unknown as
        // a match would let this definition claim a device it was not written
        // for.
        assert!(!fp.matches(&device()));
    }

    #[test]
    fn the_address_is_matched_as_a_prefix() {
        let fp = Fingerprint {
            ieee_prefix: Some("0x0017880".into()),
            ..Fingerprint::default()
        };
        let mut philips = device();
        philips.ieee = Some(Ieee::new(0x0017_8801_00dc_4d3f));
        assert!(fp.matches(&philips));

        let mut other = device();
        other.ieee = Some(Ieee::new(0x0012_4b00_2218_9abc));
        assert!(!fp.matches(&other));

        // No address learned at all cannot satisfy a prefix requirement.
        assert!(!fp.matches(&device()));
    }

    #[test]
    fn cluster_lists_compare_as_sets_not_sequences() {
        let fp = Fingerprint {
            endpoints: vec![FingerprintEndpoint {
                id: EndpointId(1),
                input_clusters: Some(vec![ClusterId(0x0000), ClusterId(0x0006)]),
                ..FingerprintEndpoint::default()
            }],
            ..Fingerprint::default()
        };
        let mut d = device();
        // Reported in the opposite order, which firmware is free to do.
        d.endpoints = vec![EndpointMatch {
            id: EndpointId(1),
            input_clusters: vec![ClusterId(0x0006), ClusterId(0x0000)],
            ..EndpointMatch::default()
        }];
        assert!(
            fp.matches(&d),
            "cluster order is a firmware detail, not part of the identity"
        );
    }

    #[test]
    fn an_extra_endpoint_makes_it_a_different_device() {
        let fp = Fingerprint {
            endpoints: vec![FingerprintEndpoint {
                id: EndpointId(1),
                ..FingerprintEndpoint::default()
            }],
            ..Fingerprint::default()
        };
        let mut one = device();
        one.endpoints = vec![EndpointMatch {
            id: EndpointId(1),
            ..EndpointMatch::default()
        }];
        assert!(fp.matches(&one));

        let mut two = device();
        two.endpoints = vec![
            EndpointMatch {
                id: EndpointId(1),
                ..EndpointMatch::default()
            },
            EndpointMatch {
                id: EndpointId(2),
                ..EndpointMatch::default()
            },
        ];
        // A two-gang switch is not a one-gang switch, and this is how upstream
        // tells them apart when they share a model string.
        assert!(!fp.matches(&two));
    }

    #[test]
    fn a_nul_padded_model_normalises_to_the_same_key() {
        // Real devices report fixed-width NUL-padded strings. Without this,
        // the same device matches or not depending on its firmware's padding.
        assert_eq!(normalise_model("TS0601\0\0\0"), "TS0601");
        assert_eq!(normalise_model("  lumi.sensor_ht  "), "lumi.sensor_ht");
        assert_eq!(index_key("TS0601\0"), "ts0601");
        assert_eq!(index_key("LUMI.Sensor_HT"), "lumi.sensor_ht");
        // A model that is only padding normalises to nothing, rather than
        // panicking or keeping the NULs.
        assert_eq!(normalise_model("\0\0"), "");
    }

    #[test]
    fn set_equality_needs_equal_length() {
        assert!(set_equal(&[1, 2], &[2, 1]));
        assert!(!set_equal(&[1, 2], &[1, 2, 3]));
        assert!(!set_equal(&[1, 2, 3], &[1, 2]));
        assert!(set_equal::<u8>(&[], &[]));
    }
}
