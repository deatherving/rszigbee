//! The definition format: what a device is, expressed as data.

use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId};

use crate::matcher::{DeviceMatch, Fingerprint, MatchRules};

/// Everything known about one device model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct Definition {
    /// The canonical model name, e.g. `TS0601_soil`.
    pub model: String,
    /// The vendor.
    pub vendor: String,
    /// A human description.
    pub description: String,
    /// How this definition claims a device.
    pub match_rules: MatchRules,
    /// What the device can do, as helper references.
    pub extend: Vec<Extend>,
    /// Tuya datapoints, for devices that speak the Tuya cluster.
    pub tuya_datapoints: Vec<TuyaDatapoint>,
    /// Bindings and attribute reporting to set up at join time.
    pub bindings: Vec<Binding>,
    /// Names for endpoints, e.g. `left` and `right` on a two-gang switch.
    ///
    /// Ordered pairs rather than a map so the definition has one canonical
    /// serialised form and a diff between upstream releases stays readable.
    pub endpoint_names: Vec<(String, EndpointId)>,
    /// Alternative branding for the same hardware.
    pub white_labels: Vec<WhiteLabel>,
    /// Whether the device supports OTA updates.
    pub ota: bool,
}

impl Definition {
    /// An empty definition for `model`.
    ///
    /// A constructor rather than a struct literal because this type is
    /// `#[non_exhaustive]`: fields stay public and mutable, so a caller in
    /// another crate builds one by starting here and assigning. Without it the
    /// type could not be constructed outside this crate at all.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }

    /// Applies any white label whose fingerprint matches, returning the
    /// branding this particular unit should be reported under.
    ///
    /// The same hardware is sold under many names, and reporting the one on the
    /// box is what makes a device recognisable to its owner. Upstream does this
    /// after resolution rather than as part of it, so a white label can never
    /// change *which* definition matched — only what it is called.
    #[must_use]
    pub fn branding(&self, device: &DeviceMatch) -> (&str, &str, &str) {
        for label in &self.white_labels {
            if label
                .fingerprints
                .iter()
                .any(|f| !f.is_empty() && f.matches(device))
            {
                return (
                    &label.model,
                    label.vendor.as_deref().unwrap_or(&self.vendor),
                    label.description.as_deref().unwrap_or(&self.description),
                );
            }
        }
        (&self.model, &self.vendor, &self.description)
    }

    /// Whether anything in this definition could not be expressed as data.
    ///
    /// This is the coverage signal. A definition carrying an
    /// [`Extend::Unsupported`] is one the transcoder could not fully express,
    /// and counting them across an upstream release is what distinguishes a
    /// sync from a fork.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self
            .extend
            .iter()
            .any(|e| matches!(e, Extend::Unsupported { .. }))
    }
}

/// Alternative branding for identical hardware.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct WhiteLabel {
    /// The model name to report.
    pub model: String,
    /// The vendor to report, or the definition's own.
    pub vendor: Option<String>,
    /// The description to report, or the definition's own.
    pub description: Option<String>,
    /// Which units this branding applies to. An empty list applies to none:
    /// branding that matched everything would rename every unit of the
    /// underlying hardware.
    pub fingerprints: Vec<Fingerprint>,
}

/// One capability a device has, named the way upstream names it.
///
/// A closed enum rather than a string plus arguments, so a definition that
/// references something this build cannot do fails to compile the generated
/// table instead of failing at runtime on a user's device. The names track
/// upstream's `modernExtend` helpers, because the transcoder maps one to one
/// and a divergent name is a mapping that has to be remembered by a person.
///
/// Ordered here by measured usage in 26.104.0, which is also the order worth
/// implementing them in: the top few carry most of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Extend {
    /// A dimmable or colour light. 1,289 upstream definitions — by far the
    /// highest-leverage single helper, and 97.4% of them are declarative.
    Light {
        /// Whether brightness is supported.
        brightness: bool,
        /// Colour temperature range in mireds, when supported.
        color_temp: Option<(u16, u16)>,
        /// Whether full colour is supported.
        color: bool,
    },
    /// `genIdentify`, so the device can be made to blink. 917 definitions.
    Identify,
    /// On/off, optionally per endpoint. 458 definitions.
    OnOff {
        /// Endpoints this applies to, empty meaning the default one.
        endpoints: Vec<EndpointId>,
        /// Whether the device reports its own state changes.
        power_on_behavior: bool,
    },
    /// Battery percentage, and low-battery. 417 definitions.
    Battery {
        /// Whether voltage is reported as well as percentage.
        voltage: bool,
    },
    /// Named endpoints. 340 definitions.
    DeviceEndpoints,
    /// Energy and power metering. 204 definitions.
    ElectricityMeter,
    /// Temperature. 194 definitions, and the first thing a grow environment
    /// needs.
    Temperature(NumericSpec),
    /// Relative humidity. 135 definitions.
    Humidity(NumericSpec),
    /// Illuminance. 111 definitions, 44% of them Tuya.
    Illuminance(NumericSpec),
    /// Soil moisture. 77% declarative upstream, and the least well served by
    /// existing stacks.
    SoilMoisture(NumericSpec),
    /// CO2 concentration.
    Co2(NumericSpec),
    /// Occupancy or presence.
    Occupancy,
    /// An IAS zone alarm: leak, smoke, contact, tamper. 141 definitions.
    IasZoneAlarm {
        /// Which alarm names this zone reports.
        alarms: Vec<String>,
    },
    /// Any other numeric attribute. 145 definitions.
    Numeric {
        /// The capability name, e.g. `co2` or `pressure`.
        name: String,
        /// Which cluster and attribute it reads.
        cluster: ClusterId,
        /// The attribute.
        attribute: AttrId,
        /// Scaling and units.
        spec: NumericSpec,
        /// Whether it can be written.
        access: Access,
    },
    /// Any other boolean attribute. 311 definitions.
    Binary {
        /// The capability name.
        name: String,
        /// Cluster.
        cluster: ClusterId,
        /// Attribute.
        attribute: AttrId,
        /// The raw value meaning true.
        value_on: i64,
        /// The raw value meaning false.
        value_off: i64,
        /// Whether it can be written.
        access: Access,
    },
    /// A named-value attribute. 132 definitions.
    EnumLookup {
        /// The capability name.
        name: String,
        /// Cluster.
        cluster: ClusterId,
        /// Attribute.
        attribute: AttrId,
        /// Raw value to name.
        values: Vec<(i64, String)>,
        /// Whether it can be written.
        access: Access,
    },

    /// A window covering: blind, shade or curtain motor.
    ///
    /// `inverted` matters more than it looks: some motors report 0 as fully
    /// open and others as fully closed, and getting it wrong means every
    /// position is reported and commanded backwards.
    WindowCovering {
        /// Whether the covering can be positioned.
        lift: bool,
        /// Whether its slats can be tilted.
        tilt: bool,
        /// Whether the device's percentage scale runs the other way.
        inverted: bool,
    },

    /// A door lock.
    ///
    /// Lock state and locking; PIN and user management are separate concerns
    /// that this does not cover.
    Lock,

    /// The device speaks Tuya's manufacturer-specific datapoint protocol.
    ///
    /// Not a capability: it is the transport those devices use *instead of*
    /// standard clusters, and the capabilities come from
    /// [`Definition::tuya_datapoints`]. It appears in 622 upstream definitions
    /// and is what makes any of them work at all.
    TuyaBase {
        /// Whether datapoint reporting is wired up.
        datapoints: bool,
        /// Whether to query the device's state when it announces itself.
        query_on_announce: bool,
        /// How often to poll, when the device does not report on its own.
        query_interval_secs: Option<u32>,
    },

    /// The device *sends* on/off commands, rather than having on/off.
    ///
    /// A remote or a wall switch: it emits commands and the coordinator
    /// receives them. That makes it an [`Access::Report`]-only source of
    /// actions, not something to be commanded — the distinction between a
    /// button and a bulb.
    CommandsOnOff {
        /// Which commands to surface, e.g. `on`, `off`, `toggle`.
        commands: Vec<String>,
        /// Endpoint names, when the device has several buttons.
        endpoints: Vec<EndpointId>,
    },

    /// Overrides what the device says its power source is.
    ///
    /// Pure metadata, and it decides whether the device is ever probed: a
    /// mains device that misreports itself as battery is never checked, and a
    /// battery device that misreports as mains is probed until it dies.
    ForcePowerSource {
        /// The truth about this device.
        source: PowerSourceHint,
    },

    /// A manufacturer-specific cluster this device has, which the standard
    /// table does not describe.
    ///
    /// Registered against the device rather than globally, because the same
    /// cluster id means different things to different manufacturers. Without
    /// it a frame from that cluster cannot be decoded at all — its attributes
    /// have no known types — so the device reports nothing usable.
    AddCustomCluster(CustomCluster),

    /// Something the transcoder could not express as data.
    ///
    /// Kept rather than dropped, and deliberately not silent. A definition
    /// carrying this is incomplete, [`Definition::is_complete`] reports that,
    /// and the note says what a person would have to implement. Dropping it
    /// instead would make the coverage number a lie and leave a device that
    /// half-works with no explanation.
    Unsupported {
        /// The upstream helper or key that could not be expressed.
        helper: String,
        /// What it would take, for whoever picks it up.
        note: String,
    },
}

/// One custom attribute: `(id, name, wire type tag)`.
///
/// The tag rather than a decoded type, so this crate stays free of the ZCL
/// type model and resolution happens once, where the registry is built.
pub type CustomAttribute = (u16, String, u8);

/// One custom command: `(id, name, ordered parameters)`.
pub type CustomCommand = (u8, String, Vec<CustomParameter>);

/// One command parameter: `(name, wire type tag)`.
pub type CustomParameter = (String, u8);

/// A manufacturer-specific cluster definition.
///
/// The same shape as a standard cluster, carried per device. The id is not
/// unique across manufacturers, which is exactly why this is scoped to the
/// devices whose definition declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct CustomCluster {
    /// The name, keeping upstream's spelling.
    pub name: String,
    /// The cluster id.
    pub id: ClusterId,
    /// The manufacturer code, when the cluster requires one on every request.
    pub manufacturer: Option<u16>,
    /// Attributes.
    pub attributes: Vec<CustomAttribute>,
    /// Client-to-server commands.
    pub commands: Vec<CustomCommand>,
    /// Server-to-client responses.
    pub responses: Vec<CustomCommand>,
}

impl Default for CustomCluster {
    fn default() -> Self {
        Self {
            name: String::new(),
            id: ClusterId(0),
            manufacturer: None,
            attributes: Vec::new(),
            commands: Vec::new(),
            responses: Vec::new(),
        }
    }
}

/// What a device is actually powered by, when it misreports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum PowerSourceHint {
    /// Mains.
    Mains,
    /// Battery.
    Battery,
    /// A DC supply.
    Dc,
}

/// Whether a capability can be read, written, or asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Access {
    /// Reported by the device; not writable.
    #[default]
    Report,
    /// Writable, and reported.
    ReportAndSet,
    /// Writable only.
    Set,
}

/// How a raw attribute value becomes a real-world quantity.
///
/// The divisor is the field that matters: ZCL carries temperature in
/// hundredths, so a missing divisor reports 2137 °C. Keeping it in the
/// definition rather than in a converter means the same mistake cannot be made
/// twice for two different devices.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct NumericSpec {
    /// Divide the raw value by this.
    pub divisor: i64,
    /// Then add this.
    pub offset: i64,
    /// The unit, for display.
    pub unit: Option<String>,
    /// Valid range, when the device declares one.
    pub range: Option<(i64, i64)>,
}

impl Default for NumericSpec {
    fn default() -> Self {
        // A divisor of one, never zero: a zero divisor would be a division by
        // zero at conversion time, and defaulting to it would make an omitted
        // field a crash rather than an identity.
        Self {
            divisor: 1,
            offset: 0,
            unit: None,
            range: None,
        }
    }
}

impl NumericSpec {
    /// Applies the scaling to a raw value.
    #[must_use]
    pub fn apply(&self, raw: i64) -> f64 {
        // Guarded rather than trusted: a definition transcoded from upstream
        // could carry a zero, and one bad definition must not take the process
        // down.
        let divisor = if self.divisor == 0 { 1 } else { self.divisor };
        #[expect(
            clippy::cast_precision_loss,
            reason = "sensor readings are far inside f64's exact integer range; \
                      the alternative is a decimal dependency for no gain"
        )]
        let scaled = raw as f64 / divisor as f64;
        #[expect(clippy::cast_precision_loss, reason = "as above")]
        let offset = self.offset as f64;
        scaled + offset
    }
}

/// One Tuya datapoint, mapped to a capability.
///
/// Tuya devices do not use standard clusters: they multiplex everything through
/// one manufacturer cluster keyed by a datapoint number. That is why they need
/// their own form — and why they cannot be skipped, since they dominate the
/// cheap-sensor end of the market that a growing setup actually buys.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct TuyaDatapoint {
    /// The datapoint number.
    pub dp: u8,
    /// The capability it maps to.
    pub name: String,
    /// How to interpret the payload.
    pub kind: TuyaKind,
    /// Which endpoint, when the device has more than one.
    pub endpoint: Option<EndpointId>,
    /// Whether it can be written.
    pub access: Access,
}

/// How a Tuya datapoint's payload is interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum TuyaKind {
    /// A boolean.
    Bool {
        /// Whether the sense is inverted.
        inverted: bool,
    },
    /// A scaled integer.
    Value(NumericSpec),
    /// A named value.
    Enum(Vec<(i64, String)>),
    /// A bitfield, one name per bit.
    Bitmap(Vec<(u8, String)>),
    /// An opaque string.
    String,
    /// Raw bytes this build cannot interpret, kept so a person can.
    Raw,
}

/// A binding and its attribute reporting, set up at join time.
///
/// Measured as the right shape: 63% of upstream `configure` bodies contain
/// nothing but calls that reduce to this table. Without reporting configured, a
/// sensor pairs, interviews, and then appears silent forever — which is the
/// single most common way a device looks broken when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct Binding {
    /// The endpoint to bind.
    pub endpoint: EndpointId,
    /// The cluster to bind.
    pub cluster: ClusterId,
    /// Attributes to configure reporting for.
    pub reporting: Vec<Reporting>,
}

impl Default for Binding {
    fn default() -> Self {
        Self {
            endpoint: EndpointId(1),
            cluster: ClusterId(0x0000),
            reporting: Vec::new(),
        }
    }
}

/// Reporting configuration for one attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[non_exhaustive]
pub struct Reporting {
    /// The attribute.
    pub attribute: AttrId,
    /// Shortest interval between reports, in seconds.
    pub min_interval: u16,
    /// Longest interval before the device reports anyway, in seconds.
    ///
    /// This is also the number availability depends on: a device that only
    /// reports on change is indistinguishable from a dead one until this
    /// elapses.
    pub max_interval: u16,
    /// Minimum change worth reporting.
    pub min_change: u64,
}

impl Default for Reporting {
    fn default() -> Self {
        Self {
            attribute: AttrId(0x0000),
            min_interval: 10,
            // An hour. Long enough not to drain a battery, short enough that a
            // silent device is noticed the same day.
            max_interval: 3600,
            min_change: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::Fingerprint;

    #[test]
    fn a_temperature_divisor_of_one_hundred_gives_degrees_not_hundredths() {
        // The exact bug this field exists to prevent: ZCL carries 21.37 degrees
        // as 2137, and a missing divisor reports 2137 degrees.
        let spec = NumericSpec {
            divisor: 100,
            ..NumericSpec::default()
        };
        assert!((spec.apply(2137) - 21.37).abs() < 1e-9);
    }

    #[test]
    fn the_default_divisor_is_one_so_an_omitted_field_is_the_identity() {
        assert_eq!(NumericSpec::default().divisor, 1);
        assert!((NumericSpec::default().apply(42) - 42.0).abs() < 1e-9);
    }

    #[test]
    fn a_zero_divisor_from_a_bad_definition_does_not_divide_by_zero() {
        let spec = NumericSpec {
            divisor: 0,
            ..NumericSpec::default()
        };
        // One bad transcoded definition must not take the process down.
        assert!((spec.apply(7) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn an_offset_applies_after_the_divisor() {
        let spec = NumericSpec {
            divisor: 100,
            offset: -273,
            unit: Some("°C".into()),
            range: None,
        };
        assert!((spec.apply(29_315) - 20.15).abs() < 1e-9);
    }

    #[test]
    fn a_definition_carrying_unsupported_is_reported_as_incomplete() {
        let mut d = Definition {
            model: "TS0601_soil".into(),
            extend: vec![Extend::Identify],
            ..Definition::default()
        };
        assert!(d.is_complete());

        d.extend.push(Extend::Unsupported {
            helper: "tuya.valueConverter.raw".into(),
            note: "needs a value converter".into(),
        });
        // The coverage signal: this is how a sync reports what it could not
        // carry, instead of quietly shipping a device that half-works.
        assert!(!d.is_complete());
    }

    #[test]
    fn a_white_label_renames_a_unit_without_changing_the_match() {
        let definition = Definition {
            model: "TS0601_soil".into(),
            vendor: "Tuya".into(),
            description: "Soil sensor".into(),
            white_labels: vec![WhiteLabel {
                model: "QT-07S".into(),
                vendor: Some("Giex".into()),
                description: None,
                fingerprints: vec![Fingerprint {
                    manufacturer_name: Some("_TZE200_myd45weu".into()),
                    ..Fingerprint::default()
                }],
            }],
            ..Definition::default()
        };

        let generic = DeviceMatch {
            model_id: Some("TS0601".into()),
            manufacturer_name: Some("_TZE200_other".into()),
            ..DeviceMatch::default()
        };
        assert_eq!(
            definition.branding(&generic),
            ("TS0601_soil", "Tuya", "Soil sensor")
        );

        let giex = DeviceMatch {
            model_id: Some("TS0601".into()),
            manufacturer_name: Some("_TZE200_myd45weu".into()),
            ..DeviceMatch::default()
        };
        // The name on the box, and the description still inherited.
        assert_eq!(
            definition.branding(&giex),
            ("QT-07S", "Giex", "Soil sensor")
        );
    }

    #[test]
    fn a_white_label_with_no_fingerprint_renames_nothing() {
        let definition = Definition {
            model: "generic".into(),
            vendor: "Vendor".into(),
            description: "Thing".into(),
            white_labels: vec![WhiteLabel {
                model: "WRONG".into(),
                ..WhiteLabel::default()
            }],
            ..Definition::default()
        };
        // An empty fingerprint list must not apply to every unit, or every
        // device of this model gets renamed.
        assert_eq!(
            definition.branding(&DeviceMatch::default()),
            ("generic", "Vendor", "Thing")
        );
    }

    #[test]
    fn reporting_defaults_notice_a_silent_device_within_a_day() {
        // A max interval of zero would mean "never report unless changed",
        // making a dead device indistinguishable from a quiet one.
        let r = Reporting::default();
        assert!(r.max_interval > 0 && r.max_interval <= 3600);
        assert!(r.min_interval < r.max_interval);
    }
}
