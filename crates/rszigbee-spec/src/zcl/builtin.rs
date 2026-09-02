//! The hand-written cluster subset for the Phase 2 vertical slice.
//!
//! **This module is temporary.** The full 129-cluster table is declarative data
//! in zigbee-herdsman (`src/zspec/zcl/definition/cluster.ts`, 7,400 lines, MIT)
//! and will be transcoded into generated Rust in Phase 3 rather than typed by
//! hand (the README credits). What is here is exactly the set
//! the vertical slice needs — enough to pair, interview and switch a plug — so
//! that the registry has real content to be tested against before the
//! generator exists.
//!
//! Cluster and attribute names deliberately match zigbee-herdsman's spelling
//! (`genOnOff`, not `on_off`), because those names appear in imported device
//! definitions, in diagnostics and in every community discussion of Zigbee
//! devices. Renaming them would make the ecosystem's accumulated knowledge
//! stop applying.

use alloc::vec::Vec;

use crate::zcl::registry::ClusterDef;
use crate::zcl::types::ZclType;

const U8: ZclType = ZclType::Uint(1);
const U16: ZclType = ZclType::Uint(2);
const U24: ZclType = ZclType::Uint(3);
const U32: ZclType = ZclType::Uint(4);
const U48: ZclType = ZclType::Uint(6);
const I16: ZclType = ZclType::Int(2);
const BOOL: ZclType = ZclType::Bool;
const ENUM8: ZclType = ZclType::Enum8;
const MAP8: ZclType = ZclType::Bitmap(1);
const STR: ZclType = ZclType::CharStr;
const IEEE: ZclType = ZclType::Ieee;

/// The clusters this build ships.
#[must_use]
pub fn clusters() -> Vec<ClusterDef> {
    alloc::vec![
        gen_basic(),
        gen_power_cfg(),
        gen_identify(),
        closures_door_lock(),
        closures_window_covering(),
        ms_illuminance(),
        ms_occupancy(),
        ms_soil_moisture(),
        ms_co2(),
        ss_ias_zone(),
        se_metering(),
        ha_electrical_measurement(),
        gen_on_off(),
        gen_level_ctrl(),
        ms_temperature(),
        ms_humidity(),
    ]
}

fn gen_basic() -> ClusterDef {
    // The attributes the interview reads, in the order it reads them.
    ClusterDef::new(0x0000, "genBasic")
        .attr(0x0000, "zclVersion", U8)
        .attr(0x0001, "appVersion", U8)
        .attr(0x0002, "stackVersion", U8)
        .attr(0x0003, "hwVersion", U8)
        .attr(0x0004, "manufacturerName", STR)
        .attr(0x0005, "modelId", STR)
        .attr(0x0006, "dateCode", STR)
        .attr(0x0007, "powerSource", ENUM8)
        .attr(0x4000, "swBuildId", STR)
        .cmd(0x00, "resetFactDefault", &[])
}

fn gen_power_cfg() -> ClusterDef {
    ClusterDef::new(0x0001, "genPowerCfg")
        .attr(0x0020, "batteryVoltage", U8)
        .attr(0x0021, "batteryPercentageRemaining", U8)
        .attr(0x0035, "batteryAlarmMask", MAP8)
        .attr(0x0036, "batteryVoltMinThres", U8)
}

fn gen_identify() -> ClusterDef {
    ClusterDef::new(0x0003, "genIdentify")
        .attr(0x0000, "identifyTime", U16)
        .cmd(0x00, "identify", &[("identifytime", U16)])
        .cmd(0x01, "identifyQuery", &[])
        .cmd(
            0x40,
            "triggerEffect",
            &[("effectid", U8), ("effectvariant", U8)],
        )
        .rsp(0x00, "identifyQueryRsp", &[("timeout", U16)])
}

fn gen_on_off() -> ClusterDef {
    ClusterDef::new(0x0006, "genOnOff")
        .attr(0x0000, "onOff", BOOL)
        .attr(0x4000, "globalSceneCtrl", BOOL)
        .attr(0x4001, "onTime", U16)
        .attr(0x4002, "offWaitTime", U16)
        .attr(0x4003, "startUpOnOff", ENUM8)
        .cmd(0x00, "off", &[])
        .cmd(0x01, "on", &[])
        .cmd(0x02, "toggle", &[])
        .cmd(
            0x40,
            "offWithEffect",
            &[("effectid", U8), ("effectvariant", U8)],
        )
        .cmd(0x41, "onWithRecallGlobalScene", &[])
        .cmd(
            0x42,
            "onWithTimedOff",
            &[("ctrlbits", MAP8), ("ontime", U16), ("offwaittime", U16)],
        )
}

fn gen_level_ctrl() -> ClusterDef {
    ClusterDef::new(0x0008, "genLevelCtrl")
        .attr(0x0000, "currentLevel", U8)
        .attr(0x0001, "remainingTime", U16)
        .attr(0x000f, "options", MAP8)
        .attr(0x0010, "onOffTransitionTime", U16)
        .attr(0x0011, "onLevel", U8)
        .attr(0x4000, "startUpCurrentLevel", U8)
        .cmd(0x00, "moveToLevel", &[("level", U8), ("transtime", U16)])
        .cmd(0x01, "move", &[("movemode", U8), ("rate", U8)])
        .cmd(
            0x02,
            "step",
            &[("stepmode", U8), ("stepsize", U8), ("transtime", U16)],
        )
        .cmd(0x03, "stop", &[])
        .cmd(
            0x04,
            "moveToLevelWithOnOff",
            &[("level", U8), ("transtime", U16)],
        )
        .cmd(0x05, "moveWithOnOff", &[("movemode", U8), ("rate", U8)])
        .cmd(
            0x06,
            "stepWithOnOff",
            &[("stepmode", U8), ("stepsize", U8), ("transtime", U16)],
        )
        .cmd(0x07, "stopWithOnOff", &[])
}

fn ms_temperature() -> ClusterDef {
    // measuredValue is int16 in centi-degrees: -1000 means -10.00 C. Getting
    // the signedness wrong here is the classic "my freezer sensor reads 655 C".
    ClusterDef::new(0x0402, "msTemperatureMeasurement")
        .attr(0x0000, "measuredValue", I16)
        .attr(0x0001, "minMeasuredValue", I16)
        .attr(0x0002, "maxMeasuredValue", I16)
        .attr(0x0003, "tolerance", U16)
}

fn ms_humidity() -> ClusterDef {
    ClusterDef::new(0x0405, "msRelativeHumidity")
        .attr(0x0000, "measuredValue", U16)
        .attr(0x0001, "minMeasuredValue", U16)
        .attr(0x0002, "maxMeasuredValue", U16)
        .attr(0x0003, "tolerance", U16)
}

/// `closuresDoorLock`. `lockState`: 0 not fully locked, 1 locked, 2 unlocked.
fn closures_door_lock() -> ClusterDef {
    ClusterDef::new(0x0101, "closuresDoorLock")
        .attr(0x0000, "lockState", ENUM8)
        .attr(0x0001, "lockType", ENUM8)
        .attr(0x0002, "actuatorEnabled", BOOL)
        .cmd(0x00, "lockDoor", &[])
        .cmd(0x01, "unlockDoor", &[])
        .cmd(0x02, "toggleDoor", &[])
}

/// `closuresWindowCovering`.
///
/// The percentage attributes are "percentage closed", not open, which is the
/// opposite of what a caller means by a position.
fn closures_window_covering() -> ClusterDef {
    ClusterDef::new(0x0102, "closuresWindowCovering")
        .attr(0x0007, "configStatus", MAP8)
        .attr(0x0008, "currentPositionLiftPercentage", U8)
        .attr(0x0009, "currentPositionTiltPercentage", U8)
        .cmd(0x00, "upOpen", &[])
        .cmd(0x01, "downClose", &[])
        .cmd(0x02, "stop", &[])
        .cmd(0x05, "goToLiftPercentage", &[("percentageliftvalue", U8)])
        .cmd(0x08, "goToTiltPercentage", &[("percentagetiltvalue", U8)])
}

/// `msIlluminanceMeasurement`.
fn ms_illuminance() -> ClusterDef {
    ClusterDef::new(0x0400, "msIlluminanceMeasurement")
        .attr(0x0000, "measuredValue", U16)
        .attr(0x0001, "minMeasuredValue", U16)
        .attr(0x0002, "maxMeasuredValue", U16)
}

/// `msOccupancySensing`. `occupancy` is a bitmap whose bit 0 is occupied.
fn ms_occupancy() -> ClusterDef {
    ClusterDef::new(0x0406, "msOccupancySensing")
        .attr(0x0000, "occupancy", MAP8)
        .attr(0x0001, "occupancySensorType", ENUM8)
}

/// `msSoilMoisture`. Not in the built-in set before, which is why a plan step
/// has to carry its own wire type.
fn ms_soil_moisture() -> ClusterDef {
    ClusterDef::new(0x0408, "msSoilMoisture")
        .attr(0x0000, "measuredValue", U16)
        .attr(0x0001, "minMeasuredValue", U16)
        .attr(0x0002, "maxMeasuredValue", U16)
}

/// `msCO2`. The measured value is a fraction of one, not parts per million.
fn ms_co2() -> ClusterDef {
    ClusterDef::new(0x040d, "msCO2").attr(0x0000, "measuredValue", ZclType::Single)
}

/// `ssIasZone`. `zoneStatus` packs alarm, tamper and battery-low into bits.
fn ss_ias_zone() -> ClusterDef {
    ClusterDef::new(0x0500, "ssIasZone")
        .attr(0x0000, "zoneState", ENUM8)
        .attr(0x0001, "zoneType", ZclType::Enum16)
        .attr(0x0002, "zoneStatus", ZclType::Bitmap(2))
        .attr(0x0010, "iasCieAddr", IEEE)
        .attr(0x0011, "zoneId", U8)
}

/// `seMetering`. The multiplier and divisor are what make the summation mean
/// anything, and reading them is part of interviewing a meter.
fn se_metering() -> ClusterDef {
    ClusterDef::new(0x0702, "seMetering")
        .attr(0x0000, "currentSummDelivered", U48)
        .attr(0x0200, "status", MAP8)
        .attr(0x0301, "multiplier", U24)
        .attr(0x0302, "divisor", U24)
        .attr(0x0400, "instantaneousDemand", ZclType::Int(3))
}

/// `haElectricalMeasurement`.
fn ha_electrical_measurement() -> ClusterDef {
    ClusterDef::new(0x0b04, "haElectricalMeasurement")
        .attr(0x0505, "rmsVoltage", U16)
        .attr(0x0508, "rmsCurrent", U16)
        .attr(0x050b, "activePower", I16)
        .attr(0x0604, "acPowerMultiplier", U16)
        .attr(0x0605, "acPowerDivisor", U16)
}

// Kept so the constants above are all exercised once the metering clusters
// land; removing them now would just mean re-adding them in Phase 3.
#[allow(dead_code)]
fn unused_type_witnesses() -> [ZclType; 4] {
    [U24, U32, U48, IEEE]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AttrId, ClusterId, CommandId};
    use crate::zcl::registry::ClusterRegistry;

    #[test]
    fn every_builtin_cluster_has_a_name_and_a_unique_id() {
        let cs = clusters();
        let mut ids: Vec<u16> = cs.iter().map(|c| c.id.0).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "duplicate cluster id in the builtin table"
        );
        assert!(cs.iter().all(|c| !c.name.is_empty()));
    }

    #[test]
    fn temperature_is_signed_so_sub_zero_readings_work() {
        // Guarding the specific mistake, not just the type.
        let reg = ClusterRegistry::with_builtins();
        let ty = reg
            .attr(None, ClusterId(0x0402), AttrId(0x0000))
            .map(|a| a.ty)
            .expect("measuredValue");
        assert_eq!(ty, ZclType::Int(2));

        let mut r = crate::codec::Reader::new(&[0x18, 0xfc]);
        assert_eq!(
            crate::zcl::types::decode_value(ty, &mut r).unwrap(),
            crate::zcl::types::ZclValue::Int(-1000)
        );
    }

    #[test]
    fn humidity_is_unsigned_because_it_cannot_be_negative() {
        let reg = ClusterRegistry::with_builtins();
        assert_eq!(
            reg.attr(None, ClusterId(0x0405), AttrId(0x0000))
                .map(|a| a.ty),
            Some(ZclType::Uint(2))
        );
    }

    #[test]
    fn on_off_command_ids_match_the_spec() {
        let reg = ClusterRegistry::with_builtins();
        let c = reg.get(None, ClusterId(0x0006)).unwrap();
        assert_eq!(c.cmd_by_name("off").map(|x| x.id), Some(CommandId(0x00)));
        assert_eq!(c.cmd_by_name("on").map(|x| x.id), Some(CommandId(0x01)));
        assert_eq!(c.cmd_by_name("toggle").map(|x| x.id), Some(CommandId(0x02)));
    }

    #[test]
    fn move_to_level_with_on_off_has_the_parameters_a_dimmer_needs() {
        let reg = ClusterRegistry::with_builtins();
        let c = reg.get(None, ClusterId(0x0008)).unwrap();
        let cmd = c.cmd_by_name("moveToLevelWithOnOff").expect("present");
        assert_eq!(cmd.id, CommandId(0x04));
        assert_eq!(cmd.params.len(), 2);
        assert_eq!(cmd.params.first().map(|p| p.name.as_str()), Some("level"));
        assert_eq!(cmd.params.first().map(|p| p.ty), Some(ZclType::Uint(1)));
        assert_eq!(cmd.params.get(1).map(|p| p.ty), Some(ZclType::Uint(2)));
    }

    #[test]
    fn the_interview_attributes_are_all_present_on_gen_basic() {
        let reg = ClusterRegistry::with_builtins();
        let c = reg.get(None, ClusterId(0x0000)).unwrap();
        for name in [
            "zclVersion",
            "appVersion",
            "stackVersion",
            "hwVersion",
            "manufacturerName",
            "modelId",
            "dateCode",
            "powerSource",
            "swBuildId",
        ] {
            assert!(c.attr_by_name(name).is_some(), "genBasic is missing {name}");
        }
    }
}
