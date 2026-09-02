//! Capability values and state deltas.

use std::collections::BTreeMap;
use std::fmt;

use crate::capability::CapabilityId;

/// A capability value.
///
/// Dynamic because a device's capabilities are described by data, not by Rust
/// types (README, "Device compatibility"). Typing happens at the edges: a
/// [`Capability`](crate::capability::Capability) declares the expected shape and
/// range, and `Capability::accepts` rejects a mismatch before it reaches a
/// device.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum StateValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// Free text.
    Str(String),
    /// One of an enumeration's declared values.
    Enum(String),
    /// A list.
    List(Vec<StateValue>),
    /// A nested map, for composite capabilities such as colour.
    Map(BTreeMap<String, StateValue>),
    /// The device reports no value. Distinct from absence: `Null` means "the
    /// sensor said it does not know", absence means "we have not heard".
    Null,
}

impl StateValue {
    /// As a boolean, if it is one.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::Enum(s) | Self::Str(s) => match s.as_str() {
                "ON" | "on" | "true" | "open" | "lock" => Some(true),
                "OFF" | "off" | "false" | "closed" | "unlock" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// As an `f64`, if it is numeric.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// True for [`StateValue::Null`].
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl fmt::Display for StateValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(x) => write!(f, "{x}"),
            Self::Str(s) | Self::Enum(s) => f.write_str(s),
            Self::List(items) => {
                f.write_str("[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Self::Map(m) => {
                f.write_str("{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str("}")
            }
            Self::Null => f.write_str("null"),
        }
    }
}

/// An ordered set of capability changes.
///
/// Ordered because command execution order matters on real hardware: a bulb
/// that is off may reject a colour change, so `state` has to move relative to
/// `brightness` depending on which way the light is going. Upstream discovered
/// this and encodes it as a sort in its MQTT layer; here it is a property of the
/// command executor, and preserving insertion order is what makes it possible.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateChanges {
    entries: Vec<(CapabilityId, StateValue)>,
}

impl StateChanges {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a value, preserving first-insertion position.
    pub fn set(&mut self, id: impl Into<CapabilityId>, value: StateValue) -> &mut Self {
        let id = id.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == id) {
            slot.1 = value;
        } else {
            self.entries.push((id, value));
        }
        self
    }

    /// Builder form of [`StateChanges::set`].
    #[must_use]
    pub fn with(mut self, id: impl Into<CapabilityId>, value: StateValue) -> Self {
        self.set(id, value);
        self
    }

    /// Looks a value up.
    #[must_use]
    pub fn get(&self, id: &CapabilityId) -> Option<&StateValue> {
        self.entries.iter().find(|(k, _)| k == id).map(|(_, v)| v)
    }

    /// True when nothing is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many changes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Iterates in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&CapabilityId, &StateValue)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    /// Merges `other` on top of `self`.
    pub fn merge(&mut self, other: &Self) {
        for (k, v) in other.iter() {
            self.set(k.clone(), v.clone());
        }
    }

    /// Reorders so the given ids come first (`front`) or last (`back`).
    ///
    /// This is the hardware ordering rule made explicit and testable, rather
    /// than a comparator buried in a publish path.
    pub fn prioritise(&mut self, ids: &[&str], position: Priority) {
        let is_listed = |k: &CapabilityId| ids.contains(&k.as_str());
        let (listed, rest): (Vec<_>, Vec<_>) = core::mem::take(&mut self.entries)
            .into_iter()
            .partition(|(k, _)| is_listed(k));
        self.entries = match position {
            Priority::Front => listed.into_iter().chain(rest).collect(),
            Priority::Back => rest.into_iter().chain(listed).collect(),
        };
    }
}

/// Where prioritised entries go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Before everything else.
    Front,
    /// After everything else.
    Back,
}

impl From<(&str, StateValue)> for StateChanges {
    fn from((k, v): (&str, StateValue)) -> Self {
        Self::new().with(k, v)
    }
}

/// A full view of a device's last known values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateSnapshot {
    values: BTreeMap<CapabilityId, StateValue>,
}

impl StateSnapshot {
    /// Empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a delta and returns the ids that actually changed.
    ///
    /// Returning only real changes is what allows publish-on-change semantics
    /// without a second comparison pass.
    pub fn apply(&mut self, changes: &StateChanges) -> Vec<CapabilityId> {
        let mut updated = Vec::new();
        for (k, v) in changes.iter() {
            if self.values.get(k) != Some(v) {
                self.values.insert(k.clone(), v.clone());
                updated.push(k.clone());
            }
        }
        updated
    }

    /// Looks a value up.
    #[must_use]
    pub fn get(&self, id: &CapabilityId) -> Option<&StateValue> {
        self.values.get(id)
    }

    /// Iterates in identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (&CapabilityId, &StateValue)> {
        self.values.iter()
    }

    /// How many values are known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True when nothing is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_preserve_insertion_order() {
        // Order is semantic, not cosmetic: it decides the order commands hit
        // the device.
        let c = StateChanges::new()
            .with("brightness", StateValue::Int(128))
            .with("state", StateValue::Enum("ON".into()))
            .with("color_temp", StateValue::Int(370));
        let ids: Vec<_> = c.iter().map(|(k, _)| k.as_str().to_owned()).collect();
        assert_eq!(ids, ["brightness", "state", "color_temp"]);
    }

    #[test]
    fn setting_an_existing_key_replaces_without_reordering() {
        let mut c = StateChanges::new()
            .with("a", StateValue::Int(1))
            .with("b", StateValue::Int(2));
        c.set("a", StateValue::Int(9));
        let ids: Vec<_> = c.iter().map(|(k, _)| k.as_str().to_owned()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(c.get(&"a".into()), Some(&StateValue::Int(9)));
    }

    #[test]
    fn prioritise_implements_the_bulb_ordering_rule() {
        // Turning a light on: state and brightness go last, so colour is applied
        // while the bulb is already on. Turning it off: they go first.
        let mut on = StateChanges::new()
            .with("state", StateValue::Enum("ON".into()))
            .with("color_temp", StateValue::Int(370))
            .with("brightness", StateValue::Int(200));
        on.prioritise(&["state", "brightness"], Priority::Back);
        let ids: Vec<_> = on.iter().map(|(k, _)| k.as_str().to_owned()).collect();
        assert_eq!(ids, ["color_temp", "state", "brightness"]);

        let mut off = StateChanges::new()
            .with("color_temp", StateValue::Int(370))
            .with("state", StateValue::Enum("OFF".into()));
        off.prioritise(&["state", "brightness"], Priority::Front);
        let ids: Vec<_> = off.iter().map(|(k, _)| k.as_str().to_owned()).collect();
        assert_eq!(ids, ["state", "color_temp"]);
    }

    #[test]
    fn a_snapshot_reports_only_genuine_changes() {
        let mut s = StateSnapshot::new();
        let changed = s.apply(&StateChanges::new().with("state", StateValue::Enum("ON".into())));
        assert_eq!(changed.len(), 1);

        // Re-applying the same value is not a change; publishing it again would
        // be noise on every consumer.
        let changed = s.apply(&StateChanges::new().with("state", StateValue::Enum("ON".into())));
        assert!(changed.is_empty());

        let changed = s.apply(&StateChanges::new().with("state", StateValue::Enum("OFF".into())));
        assert_eq!(changed.len(), 1);
    }

    #[test]
    fn null_is_a_value_distinct_from_absence() {
        let mut s = StateSnapshot::new();
        assert_eq!(s.get(&"temperature".into()), None);
        s.apply(&StateChanges::new().with("temperature", StateValue::Null));
        assert_eq!(s.get(&"temperature".into()), Some(&StateValue::Null));
        assert!(
            s.get(&"temperature".into())
                .is_some_and(StateValue::is_null)
        );
    }

    #[test]
    fn boolean_coercion_covers_the_labels_devices_and_users_actually_use() {
        assert_eq!(StateValue::Enum("ON".into()).as_bool(), Some(true));
        assert_eq!(StateValue::Str("off".into()).as_bool(), Some(false));
        assert_eq!(StateValue::Str("open".into()).as_bool(), Some(true));
        assert_eq!(StateValue::Str("unlock".into()).as_bool(), Some(false));
        // Anything unrecognised must not be guessed into a bool.
        assert_eq!(StateValue::Str("perhaps".into()).as_bool(), None);
        assert_eq!(StateValue::Int(1).as_bool(), None);
    }

    #[test]
    fn merging_lets_later_changes_win() {
        let mut a = StateChanges::new().with("x", StateValue::Int(1));
        let b = StateChanges::new()
            .with("x", StateValue::Int(2))
            .with("y", StateValue::Int(3));
        a.merge(&b);
        assert_eq!(a.get(&"x".into()), Some(&StateValue::Int(2)));
        assert_eq!(a.get(&"y".into()), Some(&StateValue::Int(3)));
        assert_eq!(a.len(), 2);
    }
}
