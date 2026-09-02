//! Checks that the transcoder's claims are true.
//!
//! `scripts/transcode-devices.mjs` decides the coverage number by declaring
//! which primitives it can emit. That declaration is a *claim about this
//! crate*, and an unchecked claim is how a coverage number drifts upward
//! without any device becoming more usable: someone adds a name to the
//! JavaScript, the report improves, and nothing works.
//!
//! So the claim is checked. Every `Extend` variant the transcoder says it emits
//! must exist here, and every Tuya converter kind it says it maps must exist
//! too. Adding a name on the JavaScript side without adding the variant fails
//! the build.
//!
//! `fixtures/claimed-primitives.json` is generated; regenerate with
//! `scripts/refresh-device-coverage.sh`.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use rszigbee_devices::{Extend, NumericSpec, TuyaKind};
use rszigbee_spec::zcl::registry::ClusterRegistry;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Claims {
    /// `Extend` variant names the transcoder emits.
    extends: Vec<String>,
    /// `TuyaKind` variant names it maps converters onto.
    #[serde(rename = "tuyaConverterKinds")]
    tuya_converter_kinds: Vec<String>,
    /// Cluster names to the ids the transcoder resolves them to.
    #[serde(default)]
    clusters: BTreeMap<String, u16>,
}

/// Builds the named `Extend` variant, or `None` if this crate has no such one.
///
/// An exhaustive match on names rather than a derive, because the point is to
/// fail when a name has no variant. Anything reachable through
/// `Extend::Unsupported` deliberately does not count: that variant is how the
/// transcoder records a *failure*, so accepting it here would let every claim
/// pass.
fn build(name: &str) -> Option<Extend> {
    Some(match name {
        "Light" => Extend::Light {
            brightness: true,
            color_temp: None,
            color: false,
        },
        "Identify" => Extend::Identify,
        "OnOff" => Extend::OnOff {
            endpoints: Vec::new(),
            power_on_behavior: false,
        },
        "Battery" => Extend::Battery { voltage: false },
        "DeviceEndpoints" => Extend::DeviceEndpoints,
        "ElectricityMeter" => Extend::ElectricityMeter,
        "Temperature" => Extend::Temperature(NumericSpec::default()),
        "Humidity" => Extend::Humidity(NumericSpec::default()),
        "Illuminance" => Extend::Illuminance(NumericSpec::default()),
        "SoilMoisture" => Extend::SoilMoisture(NumericSpec::default()),
        "Co2" => Extend::Co2(NumericSpec::default()),
        "Occupancy" => Extend::Occupancy,
        "IasZoneAlarm" => Extend::IasZoneAlarm { alarms: Vec::new() },
        "WindowCovering" => Extend::WindowCovering {
            lift: true,
            tilt: false,
            inverted: false,
        },
        "Lock" => Extend::Lock,
        "TuyaBase" => Extend::TuyaBase {
            datapoints: true,
            query_on_announce: false,
            query_interval_secs: None,
        },
        "CommandsOnOff" => Extend::CommandsOnOff {
            commands: Vec::new(),
            endpoints: Vec::new(),
        },
        "ForcePowerSource" => Extend::ForcePowerSource {
            source: rszigbee_devices::PowerSourceHint::Mains,
        },
        "Numeric" => Extend::Numeric {
            name: "x".into(),
            cluster: rszigbee_devices::reexport::ClusterId(0x0402),
            attribute: rszigbee_devices::reexport::AttrId(0x0000),
            spec: NumericSpec::default(),
            access: rszigbee_devices::Access::Report,
        },
        "Binary" => Extend::Binary {
            name: "x".into(),
            cluster: rszigbee_devices::reexport::ClusterId(0x0006),
            attribute: rszigbee_devices::reexport::AttrId(0x0000),
            value_on: 1,
            value_off: 0,
            access: rszigbee_devices::Access::Report,
        },
        "EnumLookup" => Extend::EnumLookup {
            name: "x".into(),
            cluster: rszigbee_devices::reexport::ClusterId(0x0006),
            attribute: rszigbee_devices::reexport::AttrId(0x0000),
            values: Vec::new(),
            access: rszigbee_devices::Access::Report,
        },
        _ => return None,
    })
}

/// Builds the named `TuyaKind`, or `None`.
fn build_tuya(name: &str) -> Option<TuyaKind> {
    Some(match name {
        "Bool" => TuyaKind::Bool { inverted: false },
        "Value" => TuyaKind::Value(NumericSpec::default()),
        "Enum" => TuyaKind::Enum(Vec::new()),
        "Bitmap" => TuyaKind::Bitmap(Vec::new()),
        "String" => TuyaKind::String,
        "Raw" => TuyaKind::Raw,
        _ => return None,
    })
}

#[test]
fn every_primitive_the_transcoder_claims_actually_exists() {
    let claims: Claims = serde_json::from_str(include_str!("fixtures/claimed-primitives.json"))
        .expect("claimed-primitives.json should parse");

    assert!(
        !claims.extends.is_empty(),
        "the transcoder must claim at least one primitive, or coverage is zero"
    );

    let mut unknown = Vec::new();
    for name in &claims.extends {
        if build(name).is_none() {
            unknown.push(name.clone());
        }
    }
    for name in &claims.tuya_converter_kinds {
        if build_tuya(name).is_none() {
            unknown.push(format!("TuyaKind::{name}"));
        }
    }

    assert!(
        unknown.is_empty(),
        "the transcoder claims primitives this crate does not have: {unknown:?}. \
         Either add the variant or remove the claim — leaving it inflates the \
         coverage number without making any device work."
    );
}

#[test]
fn every_cluster_id_the_transcoder_uses_matches_the_registry() {
    // A wrong id here does not fail loudly: it produces a binding to whatever
    // cluster happens to live at that number, and the device then reports
    // nothing while everything looks configured. The registry is the authority
    // for the clusters it knows.
    let claims: Claims = serde_json::from_str(include_str!("fixtures/claimed-primitives.json"))
        .expect("claimed-primitives.json should parse");
    assert!(
        !claims.clusters.is_empty(),
        "the transcoder resolves cluster names, so the map must be published"
    );

    let registry = ClusterRegistry::with_builtins();
    let mut wrong = Vec::new();
    let mut unchecked = 0;
    for (name, id) in &claims.clusters {
        match registry.get_by_name(None, name) {
            Some(def) if def.id.0 == *id => {}
            Some(def) => wrong.push(format!(
                "{name}: transcoder says 0x{id:04x}, registry says 0x{:04x}",
                def.id.0
            )),
            // The built-in registry is a hand-written subset, so it does not
            // know every cluster a binding can name. Counted rather than
            // treated as agreement.
            None => unchecked += 1,
        }
    }
    println!(
        "cluster ids checked against the registry: {} of {}, {unchecked} unknown to it",
        claims.clusters.len() - unchecked,
        claims.clusters.len()
    );
    assert!(wrong.is_empty(), "cluster id disagreements: {wrong:?}");
}

#[test]
fn unsupported_is_not_something_the_transcoder_may_claim() {
    // `Extend::Unsupported` is how a failure is recorded. If the transcoder
    // could claim it, every missing primitive would count as covered.
    assert!(
        build("Unsupported").is_none(),
        "Unsupported must never satisfy a coverage claim"
    );
}
