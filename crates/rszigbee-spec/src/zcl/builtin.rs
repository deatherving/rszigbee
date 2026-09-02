//! The cluster table this build ships.
//!
//! This module used to be a hand-written subset of seven clusters, with a note
//! saying the full table wanted transcoding rather than typing. That has now
//! happened: [`crate::zcl::generated`] carries all 129 clusters, transcoded
//! from zigbee-herdsman's own runtime definitions, and this module is the seam
//! that exposes them.
//!
//! Kept as a separate module rather than folded into the generated file so
//! that hand-curated additions have somewhere to live — a cluster the
//! ecosystem uses that upstream has not adopted, for instance — without
//! editing a file whose header says not to.

use alloc::vec::Vec;

use crate::zcl::registry::ClusterDef;

/// The clusters this build ships.
#[must_use]
pub fn clusters() -> Vec<ClusterDef> {
    crate::zcl::generated::clusters()
}

/// How many clusters this build knows.
pub const COUNT: usize = crate::zcl::generated::COUNT;

/// A regeneration that silently produced less would be a quiet loss of
/// coverage, so the floor is checked at compile time rather than in a test.
/// Upstream's table only grows.
const _: () = assert!(COUNT >= 129);

#[cfg(test)]
mod tests {
    use crate::zcl::registry::ClusterRegistry;

    #[test]
    fn the_generated_table_builds_every_cluster_it_declares() {
        assert_eq!(super::clusters().len(), super::COUNT);
    }

    #[test]
    fn the_clusters_the_runtime_depends_on_are_all_present_with_the_right_ids() {
        // Spot checks rather than a full table: these are the ones code
        // elsewhere resolves by name, so a wrong id here binds to the wrong
        // cluster and the device then reports nothing while looking configured.
        let registry = ClusterRegistry::with_builtins();
        for (name, id) in [
            ("genBasic", 0x0000u16),
            ("genPowerCfg", 0x0001),
            ("genIdentify", 0x0003),
            ("genOnOff", 0x0006),
            ("genLevelCtrl", 0x0008),
            ("closuresDoorLock", 0x0101),
            ("closuresWindowCovering", 0x0102),
            ("msIlluminanceMeasurement", 0x0400),
            ("msTemperatureMeasurement", 0x0402),
            ("msRelativeHumidity", 0x0405),
            ("msOccupancySensing", 0x0406),
            ("msSoilMoisture", 0x0408),
            ("msCO2", 0x040d),
            ("ssIasZone", 0x0500),
            ("seMetering", 0x0702),
            ("haElectricalMeasurement", 0x0b04),
        ] {
            let def = registry
                .get_by_name(None, name)
                .unwrap_or_else(|| panic!("{name} is missing from the generated table"));
            assert_eq!(def.id.0, id, "{name} has the wrong id");
        }
    }

    #[test]
    fn attribute_wire_types_come_through_so_reporting_can_be_configured() {
        // The reason the table matters at all: without a type, reporting
        // cannot be configured, because whether a reportable-change field is
        // sent depends on whether the type is analog.
        let registry = ClusterRegistry::with_builtins();
        let temperature = registry
            .attr(
                None,
                crate::ids::ClusterId(0x0402),
                crate::ids::AttrId(0x0000),
            )
            .expect("measuredValue");
        assert_eq!(temperature.name, "measuredValue");
        assert_eq!(temperature.ty, crate::zcl::types::ZclType::Int(2));

        let on_off = registry
            .attr(
                None,
                crate::ids::ClusterId(0x0006),
                crate::ids::AttrId(0x0000),
            )
            .expect("onOff");
        assert_eq!(on_off.ty, crate::zcl::types::ZclType::Bool);
    }

    #[test]
    fn a_command_with_composite_parameters_is_named_but_marked_unencodable() {
        // `genScenes.add` takes extension field sets, which have no `ZclType`.
        // Knowing its name lets a received frame be identified; the marker is
        // what stops it being encoded with an empty payload, which would be a
        // frame that is silently too short.
        let registry = ClusterRegistry::with_builtins();
        let scenes = registry
            .get_by_name(None, "genScenes")
            .expect("genScenes is in the table");
        let add = scenes.commands.get(&0x00).expect("add");
        assert_eq!(add.name, "add");
        assert!(
            add.untyped_parameters,
            "a command taking extension field sets must be marked unencodable"
        );
        assert!(add.params.is_empty());
    }

    #[test]
    fn an_ordinary_command_keeps_its_typed_parameters() {
        let registry = ClusterRegistry::with_builtins();
        let level = registry
            .get_by_name(None, "genLevelCtrl")
            .expect("genLevelCtrl");
        let move_to = level.commands.get(&0x04).expect("moveToLevelWithOnOff");
        assert!(!move_to.untyped_parameters);
        assert!(
            !move_to.params.is_empty(),
            "moveToLevelWithOnOff takes a level and a transition time"
        );
    }

    #[test]
    fn a_manufacturer_specific_cluster_keeps_its_code() {
        // A manufacturer-specific cluster is only addressable when the code is
        // sent with the request, so losing it makes every read and write fail.
        //
        // Upstream's table carries exactly one such cluster and *no*
        // per-attribute codes -- checked against its runtime data rather than
        // assumed, after an earlier version of this test assumed otherwise and
        // failed. The generator still reads the per-attribute field so that a
        // future upstream release adding one is carried rather than dropped.
        let registry = ClusterRegistry::with_builtins();
        let wwah = registry
            .get_by_name(None, "manuSpecificAmazonWWAH")
            .expect("the one manufacturer-specific cluster upstream declares");
        assert_eq!(wwah.id.0, 0xfc57);
        assert_eq!(
            wwah.manufacturer,
            Some(crate::ids::ManufacturerCode(0x1217)),
            "the cluster's manufacturer code must survive transcoding"
        );

        let with_code = super::clusters()
            .iter()
            .filter(|d| d.manufacturer.is_some())
            .count();
        assert_eq!(with_code, 1, "upstream declares exactly one");
    }
}
