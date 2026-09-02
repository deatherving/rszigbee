//! The internal capability model.
//!
//! This is deliberately **not** `Zigbee2MQTT`'s `exposes`. `exposes` is an
//! external JSON API with a raw access bitmask, stringly-typed units and the
//! endpoint name baked into the property name; reproducing it here would push
//! MQTT's shape into the core. The MQTT compatibility layer maps
//! `&[Capability]` to `exposes` (README, "Capabilities vs exposes"), and that mapping
//! is verified against thousands of real upstream definitions before any
//! hardware is involved.
//!
//! Differences that matter: typed [`Unit`] instead of a string, [`Access`] as
//! named flags instead of `0b111`, [`EndpointId`] kept separate from the
//! capability id, and [`CapabilityKind::Action`] so a button is describable
//! without inventing a fake sensor.

use std::fmt;

use rszigbee_spec::ids::EndpointId;

/// A stable capability identifier, e.g. `state`, `brightness`, `temperature`.
///
/// Stable because it appears in persisted state, in declarative definitions and
/// in application code. Renaming one is a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityId(pub String);

impl CapabilityId {
    /// Borrows the identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CapabilityId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What can be done with a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Access {
    /// The device reports it.
    pub read: bool,
    /// It can be written.
    pub write: bool,
    /// It can be polled on demand.
    pub poll: bool,
}

impl Access {
    /// Reported only.
    pub const READ: Self = Self {
        read: true,
        write: false,
        poll: false,
    };
    /// Reported and pollable.
    pub const READ_POLL: Self = Self {
        read: true,
        write: false,
        poll: true,
    };
    /// Reported, writable and pollable — the usual case for a light.
    pub const ALL: Self = Self {
        read: true,
        write: true,
        poll: true,
    };
    /// Write only, e.g. an identify trigger.
    pub const WRITE: Self = Self {
        read: false,
        write: true,
        poll: false,
    };
}

/// A typed unit. An enum rather than a string so a mapper can convert, a UI can
/// format, and a definition cannot invent `"degC"` where the rest of the
/// codebase says `"°C"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unit {
    /// Degrees Celsius.
    Celsius,
    /// Percent.
    Percent,
    /// Lux.
    Lux,
    /// Pascal.
    Pascal,
    /// Hectopascal.
    Hectopascal,
    /// Parts per million.
    Ppm,
    /// Volt.
    Volt,
    /// Ampere.
    Ampere,
    /// Watt.
    Watt,
    /// Kilowatt hour.
    KilowattHour,
    /// Hertz.
    Hertz,
    /// Second.
    Second,
    /// Minute.
    Minute,
    /// Micrograms per cubic metre.
    MicrogramsPerCubicMetre,
    /// Mireds, the reciprocal colour-temperature unit ZCL uses.
    Mired,
    /// Kelvin.
    Kelvin,
    /// Link quality indicator, `0..=255`.
    Lqi,
    /// Decibel-milliwatt.
    Dbm,
}

/// The set of values a capability accepts.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ValueDomain {
    /// A numeric range with an optional step.
    Range {
        /// Inclusive minimum.
        min: f64,
        /// Inclusive maximum.
        max: f64,
        /// Increment, when the device only accepts discrete steps.
        step: Option<f64>,
    },
    /// A fixed set of names.
    Values(Vec<String>),
    /// A boolean with named states, e.g. `("ON", "OFF")` or `("open", "closed")`.
    Binary {
        /// The true label.
        on: String,
        /// The false label.
        off: String,
    },
    /// Free-form text with an optional maximum length.
    Text {
        /// Maximum length in characters.
        max_len: Option<usize>,
    },
    /// Anything; used for escape-hatch capabilities and raw diagnostics.
    Any,
}

/// The shape of a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilityKind {
    /// On/off.
    Switch,
    /// A brightness level.
    Dimmer,
    /// Colour temperature.
    ColorTemp,
    /// Colour.
    Color,
    /// A measured or settable number.
    Numeric,
    /// A two-state value that is not a switch, e.g. `contact`, `occupancy`.
    Binary,
    /// One of a fixed set.
    Enum,
    /// Free text.
    Text,
    /// A cover with position and optionally tilt.
    Cover,
    /// A lock.
    Lock,
    /// A thermostat.
    Climate,
    /// A fan.
    Fan,
    /// A momentary event: a button press, a scene recall.
    ///
    /// Separate from state on purpose. Upstream has to fold actions into the
    /// state payload because everything ends up as one JSON object, then
    /// exclude them again through `CACHE_IGNORE_PROPERTIES`. Making the
    /// distinction structural removes that whole class of problem.
    Action,
    /// A group of sub-capabilities.
    Composite,
}

/// How a capability should be presented, when it is not primary function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// A setting the user configures.
    Config,
    /// Diagnostic information.
    Diagnostic,
}

/// One thing a device can report or be told.
#[derive(Debug, Clone, PartialEq)]
pub struct Capability {
    /// Stable identifier.
    pub id: CapabilityId,
    /// Shape.
    pub kind: CapabilityKind,
    /// What is permitted.
    pub access: Access,
    /// Unit, when numeric.
    pub unit: Option<Unit>,
    /// Accepted values.
    pub domain: ValueDomain,
    /// The endpoint this belongs to, when the device has more than one.
    ///
    /// Kept as a field rather than suffixed into `id`, unlike upstream's
    /// `state_left`. The MQTT mapper applies the suffix; the core keeps them
    /// separable so an application can ask "all `state` capabilities".
    pub endpoint: Option<EndpointId>,
    /// Human label, when a generated one would read badly.
    pub label: Option<String>,
    /// Presentation category.
    pub category: Option<Category>,
    /// Sub-capabilities, for `Composite`.
    pub features: Vec<Capability>,
}

impl Capability {
    /// A numeric capability.
    #[must_use]
    pub fn numeric(id: &str, unit: Unit, min: f64, max: f64) -> Self {
        Self {
            id: id.into(),
            kind: CapabilityKind::Numeric,
            access: Access::READ,
            unit: Some(unit),
            domain: ValueDomain::Range {
                min,
                max,
                step: None,
            },
            endpoint: None,
            label: None,
            category: None,
            features: Vec::new(),
        }
    }

    /// An on/off switch.
    #[must_use]
    pub fn switch(id: &str) -> Self {
        Self {
            id: id.into(),
            kind: CapabilityKind::Switch,
            access: Access::ALL,
            unit: None,
            domain: ValueDomain::Binary {
                on: "ON".into(),
                off: "OFF".into(),
            },
            endpoint: None,
            label: None,
            category: None,
            features: Vec::new(),
        }
    }

    /// An action capability with the set of actions it can emit.
    #[must_use]
    pub fn action(id: &str, actions: &[&str]) -> Self {
        Self {
            id: id.into(),
            kind: CapabilityKind::Action,
            access: Access::READ,
            unit: None,
            domain: ValueDomain::Values(actions.iter().map(|s| (*s).to_owned()).collect()),
            endpoint: None,
            label: None,
            category: None,
            features: Vec::new(),
        }
    }

    /// Binds this capability to an endpoint.
    #[must_use]
    pub fn on_endpoint(mut self, ep: EndpointId) -> Self {
        self.endpoint = Some(ep);
        self
    }

    /// Sets the access flags.
    #[must_use]
    pub const fn with_access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }

    /// Sets the category.
    #[must_use]
    pub const fn with_category(mut self, category: Category) -> Self {
        self.category = Some(category);
        self
    }

    /// Whether a value is acceptable for this capability.
    ///
    /// Range and enum checks live here rather than in each converter so that a
    /// device definition claiming `min: 0, max: 100` cannot be talked into
    /// sending 300 by an MQTT payload.
    // `(Any, _)` and `(_, Null)` share a body but not a meaning, and their
    // positions are load-bearing: `Any` must match before the specific domains,
    // `Null` after them. Merging them would change behaviour.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub fn accepts(&self, value: &crate::state::StateValue) -> bool {
        use crate::state::StateValue as V;
        match (&self.domain, value) {
            (ValueDomain::Any, _) => true,
            (ValueDomain::Range { min, max, .. }, V::Int(i)) => {
                #[allow(clippy::cast_precision_loss)]
                let f = *i as f64;
                f >= *min && f <= *max
            }
            (ValueDomain::Range { min, max, .. }, V::Float(f)) => f >= min && f <= max,
            (ValueDomain::Values(vs), V::Enum(s)) => vs.iter().any(|v| v == s),
            (ValueDomain::Values(vs), V::Str(s)) => vs.iter().any(|v| v == s),
            (ValueDomain::Binary { on, off }, V::Bool(_)) => !on.is_empty() && !off.is_empty(),
            (ValueDomain::Binary { on, off }, V::Enum(s) | V::Str(s)) => s == on || s == off,
            (ValueDomain::Text { max_len }, V::Str(s)) => {
                max_len.is_none_or(|m| s.chars().count() <= m)
            }
            (_, V::Null) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateValue as V;

    #[test]
    fn a_range_rejects_values_outside_it() {
        // A definition's declared range is a safety boundary, not documentation:
        // an MQTT payload must not be able to push a device past it.
        let c = Capability::numeric("brightness", Unit::Percent, 0.0, 100.0);
        assert!(c.accepts(&V::Int(0)));
        assert!(c.accepts(&V::Int(100)));
        assert!(c.accepts(&V::Float(55.5)));
        assert!(!c.accepts(&V::Int(101)));
        assert!(!c.accepts(&V::Int(-1)));
        assert!(!c.accepts(&V::Float(300.0)));
    }

    #[test]
    fn an_enum_accepts_only_its_declared_values() {
        let c = Capability {
            domain: ValueDomain::Values(vec!["low".into(), "high".into()]),
            ..Capability::numeric("mode", Unit::Percent, 0.0, 1.0)
        };
        assert!(c.accepts(&V::Enum("low".into())));
        assert!(c.accepts(&V::Str("high".into())));
        assert!(!c.accepts(&V::Enum("medium".into())));
        assert!(!c.accepts(&V::Int(1)));
    }

    #[test]
    fn a_binary_accepts_its_labels_and_bools() {
        let c = Capability::switch("state");
        assert!(c.accepts(&V::Bool(true)));
        assert!(c.accepts(&V::Enum("ON".into())));
        assert!(c.accepts(&V::Str("OFF".into())));
        assert!(!c.accepts(&V::Str("MAYBE".into())));
    }

    #[test]
    fn null_is_always_acceptable_because_devices_report_no_reading() {
        // ZCL has per-type invalid encodings; a capability must be able to hold
        // "no value" without that being a validation failure.
        let c = Capability::numeric("temperature", Unit::Celsius, -40.0, 80.0);
        assert!(c.accepts(&V::Null));
    }

    #[test]
    fn text_length_limits_are_enforced_in_characters_not_bytes() {
        let c = Capability {
            domain: ValueDomain::Text { max_len: Some(3) },
            ..Capability::numeric("name", Unit::Percent, 0.0, 1.0)
        };
        assert!(c.accepts(&V::Str("abc".into())));
        assert!(!c.accepts(&V::Str("abcd".into())));
        // Three multi-byte characters are three characters.
        assert!(c.accepts(&V::Str("温度計".into())));
    }

    #[test]
    fn endpoints_stay_separate_from_the_capability_id() {
        // Upstream encodes this as `state_left`; keeping them apart is what lets
        // an application ask for every `state` across endpoints.
        let c = Capability::switch("state").on_endpoint(EndpointId(2));
        assert_eq!(c.id.as_str(), "state");
        assert_eq!(c.endpoint, Some(EndpointId(2)));
    }

    #[test]
    fn actions_are_a_kind_not_a_numeric_sensor() {
        let c = Capability::action("action", &["single", "double", "hold"]);
        assert_eq!(c.kind, CapabilityKind::Action);
        assert!(!c.access.write);
        assert!(c.accepts(&V::Enum("double".into())));
        assert!(!c.accepts(&V::Enum("triple".into())));
    }
}
