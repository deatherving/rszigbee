//! The cluster registry: global clusters plus per-device custom clusters.
//!
//! This is the piece the README identifies as a hard requirement. Upstream's `deviceAddCustomCluster` is called 388 times across
//! zigbee-herdsman-converters, so manufacturer-specific clusters are not an
//! edge case — they are the third most common thing a device definition does.
//! A typed-module-per-cluster design cannot express "this particular device has
//! cluster `0xfc03` whose attribute 3 is a `uint16`", which is why the registry
//! is data.
//!
//! Lookups fall back from device-specific to global, so a device may both add
//! new clusters and override attributes on a standard one — which real devices
//! do (Philips overrides `genBasic`).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ids::{AttrId, ClusterId, CommandId, Ieee, ManufacturerCode};
use crate::zcl::types::ZclType;

/// An attribute definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrDef {
    /// The identifier.
    pub id: AttrId,
    /// The name used in definitions and diagnostics, e.g. `measuredValue`.
    pub name: String,
    /// The wire type.
    pub ty: ZclType,
    /// Set when the attribute is only readable with a manufacturer code.
    pub manufacturer: Option<ManufacturerCode>,
}

/// A command definition. Parameters are ordered; the payload codec walks them
/// in sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDef {
    /// The identifier.
    pub id: CommandId,
    /// The name used in definitions and diagnostics, e.g. `moveToLevel`.
    pub name: String,
    /// Ordered parameters.
    pub params: Vec<ParamDef>,
    /// Set when at least one parameter has a type this crate cannot express.
    ///
    /// Ten percent of the ZCL command set takes composite or list-valued
    /// parameters — scene extension field sets, group lists — which
    /// [`ZclType`] has no representation for. Such a command is still worth
    /// knowing by name, because that is how a received frame gets identified;
    /// what must not happen is *encoding* one, because an empty parameter list
    /// would produce a frame that is silently too short. Encoders check this
    /// and refuse.
    pub untyped_parameters: bool,
}

/// One parameter of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDef {
    /// The name used in definitions and diagnostics.
    pub name: String,
    /// The wire type.
    pub ty: ZclType,
}

/// A cluster definition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterDef {
    /// The identifier.
    pub id: ClusterId,
    /// The name used in definitions and diagnostics, e.g. `genOnOff`. These
    /// names match zigbee-herdsman's so that imported device definitions and
    /// diagnostics stay legible to anyone who knows the upstream ecosystem.
    pub name: String,
    /// Required manufacturer code for the whole cluster, if any.
    pub manufacturer: Option<ManufacturerCode>,
    /// Attributes by id.
    pub attributes: BTreeMap<u16, AttrDef>,
    /// Client-to-server commands by id.
    pub commands: BTreeMap<u8, CommandDef>,
    /// Server-to-client commands by id.
    pub responses: BTreeMap<u8, CommandDef>,
}

impl ClusterDef {
    /// A new empty cluster.
    #[must_use]
    pub fn new(id: u16, name: &str) -> Self {
        Self {
            id: ClusterId(id),
            name: name.into(),
            ..Self::default()
        }
    }

    /// Adds an attribute.
    #[must_use]
    pub fn attr(mut self, id: u16, name: &str, ty: ZclType) -> Self {
        self.attributes.insert(
            id,
            AttrDef {
                id: AttrId(id),
                name: name.into(),
                ty,
                manufacturer: None,
            },
        );
        self
    }

    /// Adds a client-to-server command.
    #[must_use]
    pub fn cmd_untyped(mut self, id: u8, name: &str) -> Self {
        self.commands.insert(
            id,
            CommandDef {
                id: CommandId(id),
                name: name.into(),
                params: Vec::new(),
                untyped_parameters: true,
            },
        );
        self
    }

    /// Declares a server-to-client response whose parameters cannot be typed.
    #[must_use]
    pub fn rsp_untyped(mut self, id: u8, name: &str) -> Self {
        self.responses.insert(
            id,
            CommandDef {
                id: CommandId(id),
                name: name.into(),
                params: Vec::new(),
                untyped_parameters: true,
            },
        );
        self
    }

    /// Declares a client-to-server command whose parameters cannot be typed.
    ///
    /// The command is named so a received frame can be identified, and marked
    /// so it cannot be encoded.
    #[must_use]
    pub fn cmd(mut self, id: u8, name: &str, params: &[(&str, ZclType)]) -> Self {
        self.commands.insert(id, Self::def(id, name, params));
        self
    }

    /// Adds a server-to-client command.
    #[must_use]
    pub fn rsp(mut self, id: u8, name: &str, params: &[(&str, ZclType)]) -> Self {
        self.responses.insert(id, Self::def(id, name, params));
        self
    }

    fn def(id: u8, name: &str, params: &[(&str, ZclType)]) -> CommandDef {
        CommandDef {
            id: CommandId(id),
            name: name.into(),
            params: params
                .iter()
                .map(|(n, t)| ParamDef {
                    name: (*n).into(),
                    ty: *t,
                })
                .collect(),
            untyped_parameters: false,
        }
    }

    /// Looks an attribute up by name.
    #[must_use]
    pub fn attr_by_name(&self, name: &str) -> Option<&AttrDef> {
        self.attributes.values().find(|a| a.name == name)
    }

    /// Looks a client-to-server command up by name.
    #[must_use]
    pub fn cmd_by_name(&self, name: &str) -> Option<&CommandDef> {
        self.commands.values().find(|c| c.name == name)
    }
}

/// Global clusters plus per-device overrides and additions.
#[derive(Debug, Default)]
pub struct ClusterRegistry {
    global: BTreeMap<u16, ClusterDef>,
    per_device: BTreeMap<Ieee, BTreeMap<u16, ClusterDef>>,
}

impl ClusterRegistry {
    /// An empty registry. Use [`ClusterRegistry::with_builtins`] for the
    /// standard clusters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry preloaded with the clusters this crate ships.
    ///
    /// The full 129-cluster table is generated from upstream data in a later
    /// phase (the README credits); the hand-written set here is the
    /// vertical-slice subset.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut me = Self::new();
        for c in crate::zcl::builtin::clusters() {
            me.insert_global(c);
        }
        me
    }

    /// Registers or replaces a global cluster.
    pub fn insert_global(&mut self, def: ClusterDef) {
        self.global.insert(def.id.0, def);
    }

    /// Registers or replaces a cluster for one device only.
    ///
    /// This is the `deviceAddCustomCluster` equivalent. Definitions declare
    /// these, and the runtime persists them with the device record so decoding
    /// works on restart before the definition has been resolved.
    pub fn insert_for_device(&mut self, ieee: Ieee, def: ClusterDef) {
        self.per_device
            .entry(ieee)
            .or_default()
            .insert(def.id.0, def);
    }

    /// Drops every custom cluster for a device.
    pub fn clear_device(&mut self, ieee: Ieee) {
        self.per_device.remove(&ieee);
    }

    /// Resolves a cluster for a device, preferring the device's own definition.
    #[must_use]
    pub fn get(&self, ieee: Option<Ieee>, id: ClusterId) -> Option<&ClusterDef> {
        ieee.and_then(|i| self.per_device.get(&i))
            .and_then(|m| m.get(&id.0))
            .or_else(|| self.global.get(&id.0))
    }

    /// Resolves a cluster by name, device-specific first.
    #[must_use]
    pub fn get_by_name(&self, ieee: Option<Ieee>, name: &str) -> Option<&ClusterDef> {
        ieee.and_then(|i| self.per_device.get(&i))
            .and_then(|m| m.values().find(|c| c.name == name))
            .or_else(|| self.global.values().find(|c| c.name == name))
    }

    /// Resolves an attribute definition for a device.
    #[must_use]
    pub fn attr(&self, ieee: Option<Ieee>, cluster: ClusterId, attr: AttrId) -> Option<&AttrDef> {
        self.get(ieee, cluster)?.attributes.get(&attr.0)
    }

    /// Number of global clusters.
    #[must_use]
    pub fn global_len(&self) -> usize {
        self.global.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ieee(n: u64) -> Ieee {
        Ieee::new(n)
    }

    #[test]
    fn builtins_load_and_are_addressable_by_id_and_name() {
        let reg = ClusterRegistry::with_builtins();
        assert!(reg.global_len() >= 6);
        let on_off = reg.get(None, ClusterId(0x0006)).expect("genOnOff present");
        assert_eq!(on_off.name, "genOnOff");
        assert_eq!(
            reg.get_by_name(None, "genOnOff").map(|c| c.id),
            Some(ClusterId(0x0006))
        );
    }

    #[test]
    fn attribute_and_command_names_resolve() {
        let reg = ClusterRegistry::with_builtins();
        let on_off = reg.get(None, ClusterId(0x0006)).unwrap();
        assert_eq!(
            on_off.attr_by_name("onOff").map(|a| a.id),
            Some(AttrId(0x0000))
        );
        assert_eq!(
            on_off.cmd_by_name("toggle").map(|c| c.id),
            Some(CommandId(0x02))
        );
        assert_eq!(
            reg.attr(None, ClusterId(0x0006), AttrId(0x0000))
                .map(|a| a.ty),
            Some(ZclType::Bool)
        );
    }

    #[test]
    fn a_device_can_add_a_manufacturer_specific_cluster() {
        // The Philips 0xfc03 case: unknown globally, known for one device.
        let mut reg = ClusterRegistry::with_builtins();
        let dev = ieee(0x0017_8801_00dc_4d3f);
        assert!(reg.get(Some(dev), ClusterId(0xfc03)).is_none());

        reg.insert_for_device(
            dev,
            ClusterDef::new(0xfc03, "manuSpecificPhilips2").cmd(
                0x00,
                "multiColor",
                &[("data", ZclType::OctStr)],
            ),
        );

        let c = reg
            .get(Some(dev), ClusterId(0xfc03))
            .expect("registered for this device");
        assert_eq!(c.name, "manuSpecificPhilips2");
        // Still invisible to other devices and to the global table.
        assert!(reg.get(Some(ieee(1)), ClusterId(0xfc03)).is_none());
        assert!(reg.get(None, ClusterId(0xfc03)).is_none());
    }

    #[test]
    fn a_device_definition_overrides_a_standard_cluster() {
        // Some vendors extend genBasic with their own attributes; the override
        // must win for that device without disturbing anyone else.
        let mut reg = ClusterRegistry::with_builtins();
        let dev = ieee(42);
        reg.insert_for_device(
            dev,
            ClusterDef::new(0x0000, "genBasic").attr(0x0031, "philipsSpecific", ZclType::Bitmap(2)),
        );

        assert!(
            reg.attr(Some(dev), ClusterId(0x0000), AttrId(0x0031))
                .is_some()
        );
        assert!(reg.attr(None, ClusterId(0x0000), AttrId(0x0031)).is_none());
        // The override replaces rather than merges, which is what upstream does
        // too; a definition that wants both must declare both.
        assert!(
            reg.attr(Some(dev), ClusterId(0x0000), AttrId(0x0005))
                .is_none()
        );
        assert!(reg.attr(None, ClusterId(0x0000), AttrId(0x0005)).is_some());
    }

    #[test]
    fn clearing_a_device_restores_the_global_view() {
        let mut reg = ClusterRegistry::with_builtins();
        let dev = ieee(7);
        reg.insert_for_device(dev, ClusterDef::new(0xfc00, "manuSpecificPhilips"));
        assert!(reg.get(Some(dev), ClusterId(0xfc00)).is_some());
        reg.clear_device(dev);
        assert!(reg.get(Some(dev), ClusterId(0xfc00)).is_none());
    }

    #[test]
    fn an_unknown_cluster_is_none_not_a_panic() {
        let reg = ClusterRegistry::with_builtins();
        assert!(reg.get(None, ClusterId(0xdead)).is_none());
        assert!(reg.attr(None, ClusterId(0xdead), AttrId(0xbeef)).is_none());
        assert!(reg.get_by_name(None, "nothingLikeThis").is_none());
    }
}
