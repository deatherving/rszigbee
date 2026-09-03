//! The escape hatch: named Rust behaviour attached to part of a definition.
//!
//! Roughly a quarter of upstream's catalogue needs behaviour a table cannot
//! express — a datapoint that unpacks into several fields, a configure step
//! with a real decision in it, a value whose meaning depends on another value.
//! The temptation is to keep growing the declarative format until it can say
//! those things, and the end of that road is a schema that has become a badly
//! designed programming language.
//!
//! So instead a definition can *name* a behaviour, and the runtime looks it up.
//!
//! ```text
//! Definition
//!   ├── declarative capabilities
//!   ├── declarative datapoints
//!   └── named behaviours  ->  Rust impl of DeviceBehavior
//! ```
//!
//! # It is local, not total
//!
//! This is the boundary that matters. A behaviour is attached to *one*
//! datapoint, or answers *one* command — it does not take the device over. A
//! Tuya thermostat with a schedule datapoint keeps every other datapoint
//! declarative, and keeps being maintained by the transcoder. If the escape
//! hatch meant "this device is now hand-written Rust", the long tail would
//! eventually eat the value of importing upstream's data at all.
//!
//! # Nothing falls back
//!
//! A behaviour returns [`Outcome::Handled`] or [`Outcome::NotHandled`], and
//! `NotHandled` means the runtime keeps looking through the behaviours the
//! definition *explicitly named*. It never drops into a generic best effort.
//! That is the same rule the rest of the runtime follows, for the same reason:
//! a guess that is right on most devices is silently wrong on the rest, and
//! those failures are the ones nobody can diagnose.

use rszigbee_devices::Definition;
use rszigbee_spec::tuya::Datapoint;

use crate::command::DeviceCommand;
use crate::device::DeviceInfo;
use crate::runtime::definitions::ConfigureStep;
use crate::state::StateChanges;

/// Whether a behaviour dealt with something.
///
/// Deliberately not `Option`: the distinction between "handled, and the answer
/// is nothing" and "not mine" has to survive, because the second means keep
/// looking and the first means stop.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome<T> {
    /// This behaviour dealt with it. The runtime stops looking.
    Handled(T),
    /// Not this behaviour's concern. The runtime tries the next one the
    /// definition named, and then gives up — it does not guess.
    NotHandled,
}

impl<T> Outcome<T> {
    /// The value, if handled.
    pub fn handled(self) -> Option<T> {
        match self {
            Self::Handled(value) => Some(value),
            Self::NotHandled => None,
        }
    }

    /// Whether this behaviour claimed it.
    #[must_use]
    pub const fn is_handled(&self) -> bool {
        matches!(self, Self::Handled(_))
    }
}

/// What a behaviour is given when decoding a Tuya datapoint.
#[derive(Debug)]
#[non_exhaustive]
pub struct DecodeContext<'a> {
    /// The device, as the interview left it.
    pub device: &'a DeviceInfo,
    /// Its definition, so a behaviour can read the rest of the table rather
    /// than hard-coding what it expects to find.
    pub definition: &'a Definition,
    /// The datapoint that arrived.
    pub datapoint: &'a Datapoint,
    /// The capability name the table gives this datapoint.
    pub capability: &'a str,
}

/// What a behaviour is given when lowering a command.
#[derive(Debug)]
#[non_exhaustive]
pub struct EncodeContext<'a> {
    /// The device.
    pub device: &'a DeviceInfo,
    /// Its definition.
    pub definition: &'a Definition,
    /// The command to lower.
    pub command: &'a DeviceCommand,
}

/// What a behaviour is given when a device is being configured.
#[derive(Debug)]
#[non_exhaustive]
pub struct ConfigureContext<'a> {
    /// The device.
    pub device: &'a DeviceInfo,
    /// Its definition.
    pub definition: &'a Definition,
}

/// Behaviour a declarative definition cannot express.
///
/// Every method defaults to [`Outcome::NotHandled`], so an implementation
/// states only what it actually does. A behaviour that handles one datapoint
/// is three lines plus the decode.
pub trait DeviceBehavior: Send + Sync + 'static {
    /// The name a definition refers to this by.
    ///
    /// Stable, because it is written into generated definitions. Renaming one
    /// silently stops a device working, so the name is part of the contract
    /// rather than a label.
    fn name(&self) -> &'static str;

    /// Converts a Tuya datapoint the table delegated here.
    fn decode_datapoint(&self, _ctx: &DecodeContext<'_>) -> Outcome<StateChanges> {
        Outcome::NotHandled
    }

    /// Lowers a command to datapoints.
    fn encode_command(&self, _ctx: &EncodeContext<'_>) -> Outcome<Vec<Datapoint>> {
        Outcome::NotHandled
    }

    /// Contributes configure steps a table cannot express.
    fn configure(&self, _ctx: &ConfigureContext<'_>) -> Outcome<Vec<ConfigureStep>> {
        Outcome::NotHandled
    }
}

/// The behaviours a runtime knows, by name.
#[derive(Default)]
pub struct BehaviorRegistry {
    behaviors: std::collections::BTreeMap<&'static str, std::sync::Arc<dyn DeviceBehavior>>,
}

impl core::fmt::Debug for BehaviorRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BehaviorRegistry")
            .field("names", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

impl BehaviorRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry with every behaviour this build ships.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.insert(super::behaviors::TuyaThermostatSchedule);
        registry
    }

    /// Adds a behaviour, replacing any with the same name.
    pub fn insert(&mut self, behavior: impl DeviceBehavior) {
        self.behaviors
            .insert(behavior.name(), std::sync::Arc::new(behavior));
    }

    /// Looks a behaviour up.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&std::sync::Arc<dyn DeviceBehavior>> {
        self.behaviors.get(name)
    }

    /// Every name known, in order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.behaviors.keys().copied()
    }

    /// How many behaviours are known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.behaviors.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.behaviors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceInfo, DeviceKind};

    struct Handles;
    impl DeviceBehavior for Handles {
        fn name(&self) -> &'static str {
            "test:handles"
        }
        fn decode_datapoint(&self, _ctx: &DecodeContext<'_>) -> Outcome<StateChanges> {
            Outcome::Handled(StateChanges::new())
        }
    }

    struct Declines;
    impl DeviceBehavior for Declines {
        fn name(&self) -> &'static str {
            "test:declines"
        }
    }

    #[test]
    fn handled_with_an_empty_result_is_not_the_same_as_not_handled() {
        // The distinction has to survive: "handled, and the answer is nothing"
        // means stop, while "not mine" means keep looking. Collapsing them to
        // `Option` loses exactly that.
        let handled: Outcome<StateChanges> = Outcome::Handled(StateChanges::new());
        assert!(handled.is_handled());
        assert!(handled.handled().is_some_and(|c| c.is_empty()));

        let declined: Outcome<StateChanges> = Outcome::NotHandled;
        assert!(!declined.is_handled());
        assert!(declined.handled().is_none());
    }

    #[test]
    fn a_behaviour_states_only_what_it_does() {
        // Every method defaults to NotHandled, so declining is the baseline
        // rather than something an implementation has to remember.
        let declines = Declines;
        assert!(
            !declines
                .encode_command(&EncodeContext {
                    device: &DeviceInfo::new(
                        rszigbee_spec::ids::Ieee::new(1),
                        rszigbee_spec::ids::Nwk::new(1),
                        DeviceKind::Unknown,
                    ),
                    definition: &Definition::new("x"),
                    command: &DeviceCommand::Toggle,
                })
                .is_handled()
        );
    }

    #[test]
    fn the_registry_looks_behaviours_up_by_the_name_they_declare() {
        let mut registry = BehaviorRegistry::new();
        registry.insert(Handles);
        registry.insert(Declines);
        assert_eq!(registry.len(), 2);
        assert!(registry.get("test:handles").is_some());
        assert!(registry.get("test:declines").is_some());
        // A name nothing declared is absent, not a default.
        assert!(registry.get("test:missing").is_none());
    }

    #[test]
    fn the_builtin_registry_ships_the_behaviours_definitions_can_name() {
        let registry = BehaviorRegistry::with_builtins();
        assert!(!registry.is_empty());
        assert!(
            registry.get("tuya:thermostat-schedule").is_some(),
            "known names: {:?}",
            registry.names().collect::<Vec<_>>()
        );
    }
}
