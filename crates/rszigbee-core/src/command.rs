//! The command model: capability-level commands with lower-level escape
//! hatches, per the the README design notes.

use std::time::Duration;

use rszigbee_adapter::TxFailure;
use rszigbee_spec::ids::{AttrId, ClusterId, CommandId, EndpointId};
use rszigbee_spec::zcl::ZclValue;

use crate::capability::CapabilityId;
use crate::state::{StateChanges, StateValue};

/// A brightness level, `0..=254` as ZCL defines it.
///
/// A newtype because `u8` invites both the 0-100 and the 0-255 mistake, and
/// because 255 is not a valid level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Brightness(u8);

impl Brightness {
    /// Off.
    pub const MIN: Self = Self(0);
    /// Maximum.
    pub const MAX: Self = Self(254);

    /// Clamps a raw level into range.
    #[must_use]
    pub const fn new(level: u8) -> Self {
        Self(if level > 254 { 254 } else { level })
    }

    /// From a percentage, `0..=100`.
    #[must_use]
    pub fn from_percent(pct: u8) -> Self {
        let pct = u32::from(pct.min(100));
        // Round to nearest rather than truncating: 50 % should be 127, not 126.
        // `pct <= 100` bounds the result at 254, so the conversion cannot fail;
        // the fallback is there so this stays panic-free by construction.
        Self(u8::try_from((pct * 254 + 50) / 100).unwrap_or(Self::MAX.0))
    }

    /// The raw level.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// As a percentage, rounded.
    #[must_use]
    pub const fn to_percent(self) -> u8 {
        // `self.0 <= 254` bounds the numerator at 25 527, so the u16 arithmetic
        // cannot overflow and the result is always in `0..=100`.
        let pct = (self.0 as u16 * 100 + 127) / 254;
        #[allow(clippy::cast_possible_truncation)]
        let pct = pct as u8;
        pct
    }
}

/// Colour temperature in mireds, the reciprocal unit ZCL uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mireds(pub u16);

impl Mireds {
    /// From kelvin. Saturates rather than dividing by zero.
    #[must_use]
    pub const fn from_kelvin(k: u32) -> Self {
        if k == 0 {
            return Self(u16::MAX);
        }
        let m = 1_000_000 / k;
        // Explicitly clamped above, so the narrowing cannot lose information.
        #[allow(clippy::cast_possible_truncation)]
        let clamped = if m > u16::MAX as u32 {
            u16::MAX
        } else {
            m as u16
        };
        Self(clamped)
    }

    /// To kelvin.
    #[must_use]
    pub const fn to_kelvin(self) -> u32 {
        if self.0 == 0 {
            0
        } else {
            1_000_000 / self.0 as u32
        }
    }
}

/// A colour, in whichever space the caller has.
///
/// The definition's metadata decides what actually goes on the wire — some
/// bulbs want xy, some want hue/saturation, some need the enhanced-hue command
/// or a red-shift correction. That is a converter concern, not the caller's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// CIE 1931 xy, both `0.0..=1.0`.
    Xy {
        /// x.
        x: f64,
        /// y.
        y: f64,
    },
    /// Hue `0..=360` degrees and saturation `0..=100` percent.
    HueSat {
        /// Hue in degrees.
        hue: f64,
        /// Saturation in percent.
        saturation: f64,
    },
    /// 8-bit sRGB.
    Rgb {
        /// Red.
        r: u8,
        /// Green.
        g: u8,
        /// Blue.
        b: u8,
    },
}

/// A percentage, `0..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percent(u8);

impl Percent {
    /// Clamps into range.
    #[must_use]
    pub const fn new(v: u8) -> Self {
        Self(if v > 100 { 100 } else { v })
    }

    /// The value.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }
}

/// A raw ZCL command, the first escape hatch.
#[derive(Debug, Clone, PartialEq)]
pub struct ZclCommand {
    /// Endpoint, or `None` for the definition's default.
    pub endpoint: Option<EndpointId>,
    /// Cluster.
    pub cluster: ClusterId,
    /// Command.
    pub command: CommandId,
    /// Parameters, by name, resolved against the registry.
    pub params: Vec<(String, ZclValue)>,
    /// Manufacturer code, when the command needs one.
    pub manufacturer: Option<u16>,
    /// Whether to suppress the Default Response.
    pub disable_default_response: bool,
}

/// A raw attribute write, the second escape hatch.
#[derive(Debug, Clone, PartialEq)]
pub struct ZclAttributeWrite {
    /// Endpoint, or `None` for the definition's default.
    pub endpoint: Option<EndpointId>,
    /// Cluster.
    pub cluster: ClusterId,
    /// Attributes to write.
    pub attributes: Vec<(AttrId, ZclValue)>,
    /// Manufacturer code, when required.
    pub manufacturer: Option<u16>,
}

/// Something to ask a device to do.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DeviceCommand {
    /// The general form: write these capability values.
    Set(StateChanges),
    /// Read these capabilities from the device.
    Get(Vec<CapabilityId>),

    // Ergonomic constructors. Each lowers to `Set`, so there is exactly one
    // execution path and the convenience cannot drift from the general case.
    /// On or off.
    SetOn(bool),
    /// Toggle.
    Toggle,
    /// Set brightness.
    SetBrightness(Brightness),
    /// Set colour temperature.
    SetColorTemp(Mireds),
    /// Set colour.
    SetColor(Color),
    /// Set a cover position.
    SetPosition(Percent),
    /// Set a cover tilt.
    SetTilt(Percent),
    /// Open a cover.
    Open,
    /// Close a cover.
    Close,
    /// Stop a cover.
    Stop,
    /// Lock.
    Lock,
    /// Unlock.
    Unlock,
    /// Set a thermostat target.
    SetTargetTemperature(f64),
    /// Select a preset.
    SetPreset(String),
    /// Make the device identify itself.
    Identify {
        /// For how long.
        duration: Duration,
    },

    // Escape hatches.
    /// Send an arbitrary ZCL command.
    Zcl(ZclCommand),
    /// Write arbitrary attributes.
    ZclAttributes(ZclAttributeWrite),
}

impl DeviceCommand {
    /// Lowers an ergonomic variant to its `Set` form.
    ///
    /// Returns `None` for `Get` and the escape hatches, which are not
    /// capability writes.
    #[must_use]
    pub fn to_changes(&self) -> Option<StateChanges> {
        let c = StateChanges::new;
        Some(match self {
            Self::Set(s) => s.clone(),
            Self::SetOn(on) => c().with(
                "state",
                StateValue::Enum(if *on { "ON".into() } else { "OFF".into() }),
            ),
            Self::Toggle => c().with("state", StateValue::Enum("TOGGLE".into())),
            Self::SetBrightness(b) => c().with("brightness", StateValue::Int(i64::from(b.raw()))),
            Self::SetColorTemp(m) => c().with("color_temp", StateValue::Int(i64::from(m.0))),
            Self::SetColor(_) => c().with("color", StateValue::Null),
            Self::SetPosition(p) => c().with("position", StateValue::Int(i64::from(p.raw()))),
            Self::SetTilt(p) => c().with("tilt", StateValue::Int(i64::from(p.raw()))),
            Self::Open => c().with("state", StateValue::Enum("OPEN".into())),
            Self::Close => c().with("state", StateValue::Enum("CLOSE".into())),
            Self::Stop => c().with("state", StateValue::Enum("STOP".into())),
            Self::Lock => c().with("state", StateValue::Enum("LOCK".into())),
            Self::Unlock => c().with("state", StateValue::Enum("UNLOCK".into())),
            Self::SetTargetTemperature(t) => {
                c().with("occupied_heating_setpoint", StateValue::Float(*t))
            }
            Self::SetPreset(p) => c().with("preset", StateValue::Enum(p.clone())),
            // Identify is a momentary action, and reads and escape hatches are
            // not capability writes — none of them lower to a state change.
            Self::Identify { .. } | Self::Get(_) | Self::Zcl(_) | Self::ZclAttributes(_) => {
                return None;
            }
        })
    }
}

/// What happened to a command at the Zigbee layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// The device acknowledged.
    Acked,
    /// Sent, with no acknowledgement requested.
    NoResponseRequested,
    /// Buffered for a sleeping device; it will be delivered on next check-in.
    ///
    /// This variant is why sleepy devices work. Without it the only honest
    /// answers are "success" (a lie) or "timeout" (also a lie), and callers end
    /// up retrying commands that were already queued.
    Queued,
}

/// The result of a command.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutcome {
    /// What the converter says the state became, when it can say. This is what
    /// the MQTT layer publishes optimistically.
    pub optimistic_state: Option<StateChanges>,
    /// Delivery outcome.
    pub confirmed: Confirmation,
    /// How long it took.
    pub latency: Duration,
}

/// Why a command could not be carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    /// No such device.
    #[error("device {0} is not known")]
    UnknownDevice(rszigbee_spec::ids::Ieee),
    /// The device has no definition, so a capability write cannot be mapped.
    #[error("device has no resolved definition; use the ZCL escape hatch instead")]
    NoDefinition,
    /// The device's definition has no such capability.
    #[error("device does not support capability '{0}'")]
    UnsupportedCapability(CapabilityId),
    /// The value is outside the capability's declared domain.
    #[error("value {value} is not valid for capability '{capability}'")]
    InvalidValue {
        /// Which capability.
        capability: CapabilityId,
        /// The offending value, rendered.
        value: String,
    },
    /// The device has no such endpoint.
    #[error("device has no endpoint {0}")]
    UnknownEndpoint(EndpointId),
    /// Delivery failed.
    #[error("delivery failed: {0}")]
    Delivery(#[from] TxFailure),
    /// The deadline expired.
    #[error("command timed out after {0:?}")]
    Timeout(Duration),
    /// The runtime is shutting down.
    #[error("the runtime is shutting down")]
    ShuttingDown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brightness_percentages_round_to_nearest() {
        assert_eq!(Brightness::from_percent(0).raw(), 0);
        assert_eq!(Brightness::from_percent(100).raw(), 254);
        // 50 % should land on 127, not 126 — truncation here is visible to users
        // as a light that never quite reaches half.
        assert_eq!(Brightness::from_percent(50).raw(), 127);
        // Out-of-range input is clamped, never wrapped.
        assert_eq!(Brightness::from_percent(200).raw(), 254);
    }

    #[test]
    fn brightness_never_exceeds_the_zcl_maximum() {
        assert_eq!(Brightness::new(255).raw(), 254);
        assert_eq!(Brightness::MAX.raw(), 254);
    }

    #[test]
    fn brightness_percent_round_trips_within_one_point() {
        for pct in 0u8..=100 {
            let back = Brightness::from_percent(pct).to_percent();
            assert!(back.abs_diff(pct) <= 1, "{pct} -> {back}");
        }
    }

    #[test]
    fn mireds_and_kelvin_convert_without_dividing_by_zero() {
        assert_eq!(Mireds::from_kelvin(2700).0, 370);
        assert_eq!(Mireds::from_kelvin(6500).0, 153);
        assert_eq!(Mireds(370).to_kelvin(), 2702);
        // Degenerate inputs saturate rather than panicking.
        assert_eq!(Mireds::from_kelvin(0).0, u16::MAX);
        assert_eq!(Mireds(0).to_kelvin(), 0);
    }

    #[test]
    fn percentages_clamp() {
        assert_eq!(Percent::new(200).raw(), 100);
        assert_eq!(Percent::new(50).raw(), 50);
    }

    #[test]
    fn ergonomic_commands_lower_to_the_general_set_form() {
        // One execution path: the shortcuts cannot drift from Set.
        let on = DeviceCommand::SetOn(true).to_changes().unwrap();
        assert_eq!(
            on.get(&"state".into()),
            Some(&StateValue::Enum("ON".into()))
        );

        let b = DeviceCommand::SetBrightness(Brightness::from_percent(100))
            .to_changes()
            .unwrap();
        assert_eq!(b.get(&"brightness".into()), Some(&StateValue::Int(254)));

        let ct = DeviceCommand::SetColorTemp(Mireds::from_kelvin(2700))
            .to_changes()
            .unwrap();
        assert_eq!(ct.get(&"color_temp".into()), Some(&StateValue::Int(370)));
    }

    #[test]
    fn reads_and_escape_hatches_are_not_capability_writes() {
        assert!(
            DeviceCommand::Get(vec!["state".into()])
                .to_changes()
                .is_none()
        );
        assert!(
            DeviceCommand::Zcl(ZclCommand {
                endpoint: None,
                cluster: ClusterId(0x0006),
                command: CommandId(0x02),
                params: vec![],
                manufacturer: None,
                disable_default_response: false,
            })
            .to_changes()
            .is_none()
        );
        assert!(
            DeviceCommand::Identify {
                duration: Duration::from_secs(3)
            }
            .to_changes()
            .is_none()
        );
    }

    #[test]
    fn queued_is_distinguishable_from_success_and_failure() {
        // Reporting a buffered command to a sleeping device as either would make
        // callers retry something already in flight.
        assert_ne!(Confirmation::Queued, Confirmation::Acked);
        assert_ne!(Confirmation::Queued, Confirmation::NoResponseRequested);
    }

    #[test]
    fn errors_say_what_to_do_next() {
        let e = CommandError::NoDefinition;
        assert!(e.to_string().contains("escape hatch"));
        let e = CommandError::UnsupportedCapability("brightness".into());
        assert_eq!(
            e.to_string(),
            "device does not support capability 'brightness'"
        );
    }
}
