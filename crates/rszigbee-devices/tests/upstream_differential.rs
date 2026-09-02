//! Differential test against zigbee-herdsman-converters' own resolver.
//!
//! Reading upstream's algorithm and reimplementing it is not evidence that the
//! reimplementation agrees with it. This test is the evidence: every definition's
//! match rules and every answer in `fixtures/` were produced by *running*
//! zhc 26.104.0's `findByDevice`, not by reading its source. If our resolution
//! differs for any device, this fails and names it.
//!
//! Why it matters more than it looks: a device that resolves here to a different
//! definition than it does upstream behaves differently for no reason its owner
//! can see. The whole value of transcoding upstream's data is that the behaviour
//! comes with it, and that only holds if resolution agrees.
//!
//! `fixtures/` is derived from zigbee-herdsman-converters (MIT, © 2018 Koen
//! Kanters) and is test-only data. Regenerate with
//! `scripts/refresh-device-fixtures.sh`.
//!
//! # What is and is not covered
//!
//! The probes are every distinct `(modelID, manufacturerName)` pair upstream
//! knows, plus deliberate misses. Fingerprints keyed on an endpoint layout, an
//! address pattern, or a firmware version are **excluded** here, because
//! generating a faithful device stub for each is a bigger job than the unit
//! tests it would duplicate — those rules are covered in `matcher.rs`.

// This whole file is a test harness. The parse-path lints exist to keep a
// malformed radio frame from taking the process down; a failed `expect` on a
// fixture here is how the harness reports a broken fixture. `clippy.toml`
// relaxes them inside `#[test]` functions, which does not reach the helpers
// this file factors out.
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use rszigbee_devices::{
    Definition, DefinitionIndex, DeviceMatch, Fingerprint, MatchRules, WhiteLabel,
};
use serde::Deserialize;

/// One definition's match rules, in the shape the fixture stores.
#[derive(Debug, Deserialize)]
struct Rules {
    /// Canonical model name.
    m: String,
    /// `zigbeeModel` strings.
    #[serde(default)]
    z: Vec<String>,
    /// Fingerprints.
    #[serde(default)]
    f: Vec<RawFingerprint>,
    /// White labels, which rename the result without changing which definition
    /// matched.
    #[serde(default)]
    w: Vec<RawWhiteLabel>,
}

/// A white label, with upstream's field names.
#[derive(Debug, Deserialize)]
struct RawWhiteLabel {
    model: String,
    vendor: Option<String>,
    description: Option<String>,
    #[serde(default)]
    fingerprints: Vec<RawFingerprint>,
}

/// A fingerprint with upstream's field names.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFingerprint {
    #[serde(rename = "modelID", default)]
    model_id: Option<String>,
    #[serde(default)]
    manufacturer_name: Option<String>,
    #[serde(rename = "manufacturerID", default)]
    manufacturer_id: Option<u16>,
    #[serde(default)]
    application_version: Option<u8>,
    #[serde(default)]
    stack_version: Option<u8>,
    #[serde(rename = "zclVersion", default)]
    zcl_version: Option<u8>,
    #[serde(default)]
    hardware_version: Option<u8>,
    #[serde(default)]
    date_code: Option<String>,
    #[serde(rename = "softwareBuildID", default)]
    software_build_id: Option<String>,
    #[serde(default)]
    power_source: Option<String>,
    #[serde(rename = "type", default)]
    device_type: Option<String>,
    #[serde(rename = "ieeeAddr", default)]
    ieee_addr: Option<String>,
    #[serde(default)]
    endpoints: Option<serde_json::Value>,
    #[serde(default)]
    priority: Option<i32>,
}

/// One probe: what the device reported, and what upstream answered.
///
/// `(modelID, manufacturerName, resolved model, model after branding)`. The
/// last two are kept apart because they test different things: which definition
/// matched, and what the unit ends up called.
type Probe = (String, Option<String>, Option<String>, Option<String>);

/// Whether this fingerprint uses a field the probe stubs do not model.
///
/// Such a rule can never match in this test, so carrying it would make our
/// resolver consider a candidate upstream also rejected — harmless — but
/// carrying it *without* its constraint would make us match where upstream did
/// not. It is dropped rather than approximated.
fn beyond_probe_scope(raw: &RawFingerprint) -> bool {
    raw.endpoints.is_some() || raw.ieee_addr.is_some() || raw.application_version.is_some()
}

/// Maps upstream's string enums to the raw values our matcher compares.
///
/// Interned to small integers rather than kept as strings so `DeviceMatch`
/// stays cheap; the mapping only has to be *consistent* between the fixture and
/// the probe, which is what this test checks.
fn intern(table: &mut BTreeMap<String, u8>, value: &str) -> u8 {
    let next = u8::try_from(table.len()).unwrap_or(u8::MAX);
    *table.entry(value.to_owned()).or_insert(next)
}

/// Converts one harvested fingerprint into ours.
fn convert(
    raw: &RawFingerprint,
    power: &mut BTreeMap<String, u8>,
    kinds: &mut BTreeMap<String, u8>,
) -> Fingerprint {
    let mut fp = Fingerprint::default();
    fp.model_id.clone_from(&raw.model_id);
    fp.manufacturer_name.clone_from(&raw.manufacturer_name);
    fp.manufacturer_id = raw.manufacturer_id;
    fp.application_version = raw.application_version;
    fp.stack_version = raw.stack_version;
    fp.zcl_version = raw.zcl_version;
    fp.hardware_version = raw.hardware_version;
    fp.date_code.clone_from(&raw.date_code);
    fp.software_build_id.clone_from(&raw.software_build_id);
    fp.power_source = raw.power_source.as_deref().map(|v| intern(power, v));
    fp.device_type = raw.device_type.as_deref().map(|v| intern(kinds, v));
    fp.priority = raw.priority.unwrap_or(0);
    fp
}

#[test]
fn resolution_agrees_with_upstream_for_every_known_device() {
    let rules: Vec<Rules> = serde_json::from_str(include_str!("fixtures/match-rules.json"))
        .expect("match-rules.json should parse");
    let probes: Vec<Probe> = serde_json::from_str(include_str!("fixtures/expected.json"))
        .expect("expected.json should parse");

    assert!(
        rules.len() > 4000,
        "the fixture should carry upstream's whole catalogue, got {}",
        rules.len()
    );
    assert!(probes.len() > 6000, "too few probes: {}", probes.len());

    let mut power = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    let (index, refused) = build_index(&rules, &mut power, &mut kinds);
    compare(&index, &probes, &mut kinds, refused);
}

/// Builds an index from the harvested match rules.
///
/// Returns the index and how many definitions were skipped because every rule
/// they had is out of probe scope.
fn build_index(
    rules: &[Rules],
    power: &mut BTreeMap<String, u8>,
    kinds: &mut BTreeMap<String, u8>,
) -> (DefinitionIndex, usize) {
    let mut index = DefinitionIndex::new();
    let mut refused = 0;
    for rule in rules {
        let fingerprints: Vec<Fingerprint> = rule
            .f
            .iter()
            .filter(|raw| !beyond_probe_scope(raw))
            .map(|raw| convert(raw, power, kinds))
            .filter(|f| !f.is_empty())
            .collect();

        if rule.z.is_empty() && fingerprints.is_empty() {
            // Every rule this definition had is out of probe scope, so no probe
            // could reach it either way.
            refused += 1;
            continue;
        }

        let mut definition = Definition::new(rule.m.clone());
        definition.match_rules = MatchRules {
            models: rule.z.clone(),
            fingerprints,
        };
        for raw in &rule.w {
            let mut label = WhiteLabel::default();
            label.model.clone_from(&raw.model);
            label.vendor.clone_from(&raw.vendor);
            label.description.clone_from(&raw.description);
            label.fingerprints = raw
                .fingerprints
                .iter()
                .filter(|f| !beyond_probe_scope(f))
                .map(|f| convert(f, power, kinds))
                .filter(|f| !f.is_empty())
                .collect();
            definition.white_labels.push(label);
        }
        index
            .insert(definition)
            .expect("every fixture definition should be insertable");
    }
    (index, refused)
}

/// Compares our resolution and branding against upstream's, for every probe.
fn compare(
    index: &DefinitionIndex,
    probes: &[Probe],
    kinds: &mut BTreeMap<String, u8>,
    refused: usize,
) {
    let mut agreed = 0;
    let mut both_none = 0;
    let mut branding_agreed = 0;
    let mut disagreements = Vec::new();
    let mut branding_disagreements = Vec::new();

    let end_device = intern(kinds, "EndDevice");
    for (model_id, manufacturer_name, upstream, upstream_branded) in probes {
        let mut device = DeviceMatch::for_model(model_id.clone());
        device.manufacturer_name.clone_from(manufacturer_name);
        // Matching what the extraction stub reported, so a fingerprint on
        // either field is exercised rather than silently skipped.
        device.device_type = Some(end_device);

        let resolved = index.resolve(&device);
        let ours = resolved.map(|d| d.model.clone());

        match (ours.as_deref(), upstream.as_deref()) {
            (Some(a), Some(b)) if a == b => agreed += 1,
            (None, None) => both_none += 1,
            (a, b) => disagreements.push((
                model_id.clone(),
                manufacturer_name.clone(),
                a.map(str::to_owned),
                b.map(str::to_owned),
            )),
        }

        // Branding: same definition, but the name reported for this unit. 647
        // of these probes are renamed by a white label, so this is not a
        // formality.
        let ours_branded = resolved.map(|d| d.branding(&device).0.to_owned());
        match (ours_branded.as_deref(), upstream_branded.as_deref()) {
            (Some(a), Some(b)) if a == b => branding_agreed += 1,
            (None, None) => {}
            (a, b) => branding_disagreements.push((
                model_id.clone(),
                manufacturer_name.clone(),
                a.map(str::to_owned),
                b.map(str::to_owned),
            )),
        }
    }

    let total = probes.len();
    println!(
        "definitions indexed: {} ({refused} entirely out of probe scope)\n\
         probes: {total}\n\
         resolution: agreed {agreed}, both no-match {both_none}, disagreed {}\n\
         branding:   agreed {branding_agreed}, disagreed {}",
        index.len(),
        disagreements.len(),
        branding_disagreements.len()
    );

    for (label, list) in [
        ("resolution", &disagreements),
        ("branding", &branding_disagreements),
    ] {
        for (model, manufacturer, ours, upstream) in list.iter().take(20) {
            println!(
                "  {label}: {model} / {}: ours={} upstream={}",
                manufacturer.as_deref().unwrap_or("-"),
                ours.as_deref().unwrap_or("NO MATCH"),
                upstream.as_deref().unwrap_or("NO MATCH")
            );
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} of {total} devices resolve to a different definition than upstream",
        disagreements.len()
    );
    assert!(
        branding_disagreements.is_empty(),
        "{} of {total} devices are reported under a different name than upstream",
        branding_disagreements.len()
    );
}
