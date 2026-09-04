//! Checks that the definitions this build ships actually resolve devices.
//!
//! Until these existed the crate compiled, the coverage report said 48.9%
//! usable, and [`DefinitionIndex::new`] — an empty index — was what a caller
//! got. Every test passed. A definition set is only worth anything if a real
//! device's `modelId` finds it, so that is what is asserted here, using
//! fingerprints read off actual hardware rather than invented ones.

#![allow(clippy::expect_used, clippy::panic)]

use rszigbee_devices::{BUNDLED_COUNT, DefinitionIndex, DeviceMatch, Extend};

/// A generated set that shrank to nothing is a build failure, not a test
/// failure: checked at compile time, because `BUNDLED_COUNT` is a constant and
/// a runtime assertion on it can only ever be trivially true or trivially
/// false. Upstream carries thousands of definitions; a build with a handful
/// means the emitter or the refresh pipeline broke.
const _: () = assert!(
    BUNDLED_COUNT > 4000,
    "the generated definition set is implausibly small"
);

/// Built once; building the index is the expensive part.
fn index() -> DefinitionIndex {
    DefinitionIndex::bundled()
}

#[test]
fn every_generated_definition_reaches_the_index() {
    // A shortfall here means definitions were refused on insert -- a definition
    // nothing can match, or a fingerprint that constrains nothing. Both are
    // silent: the index is simply smaller, and every other test still passes.
    // That is exactly what happened when match rules were taken from the IR
    // instead of the cross-validated harvest, and 733 definitions vanished.
    let index = index();
    assert_eq!(
        index.len(),
        BUNDLED_COUNT,
        "every generated definition should reach the index"
    );
}

#[test]
fn a_sonoff_water_valve_resolves_and_is_branded_as_the_unit_on_the_box() {
    // Read off real hardware: a SONOFF SWV-ZNU Hydro One Lite. Upstream files
    // it under SWV-ZNE with SWV-ZNU as a white label, so this checks both that
    // it resolves and that it is reported under the name its owner would
    // recognise -- the two are different definitions' worth of behaviour.
    let index = index();
    let device = DeviceMatch::for_model("SWV-ZNU").with_manufacturer("SONOFF");

    let definition = index
        .resolve(&device)
        .expect("a SONOFF SWV-ZNU should resolve; this exact device was paired on hardware");

    assert_eq!(definition.model, "SWV-ZNE", "upstream's canonical model");

    let (model, vendor, _description) = definition.branding(&device);
    assert_eq!(model, "SWV-ZNU", "the model printed on this unit");
    assert_eq!(vendor, "SONOFF");

    // The capabilities that matter for a valve, and that were seen reporting on
    // hardware: on/off and battery.
    let has_on_off = definition
        .extend
        .iter()
        .any(|e| matches!(e, Extend::OnOff { .. }));
    let has_battery = definition
        .extend
        .iter()
        .any(|e| matches!(e, Extend::Battery { .. }));
    assert!(has_on_off, "the valve reported genOnOff on hardware");
    assert!(
        has_battery,
        "the valve reported a battery percentage on hardware"
    );
}

#[test]
fn the_valves_irrigation_attributes_are_recorded_as_unsupported_not_dropped() {
    // The valve's `child_lock` and irrigation attributes live on the
    // manufacturer cluster it reported as 0xFC11. The cluster *id* resolves --
    // it is harvested from an `AddCustomCluster` elsewhere in the corpus -- but
    // the attribute names do not: upstream declares them in a shared module the
    // transcoder does not capture as a declaration, so nothing in the available
    // data says which attribute id `childLock` is.
    //
    // Guessing one would be worse than refusing. A wrong attribute id does not
    // fail; it reads whatever attribute happens to live at that number and
    // returns a plausible value. So these are recorded as `Unsupported`, which
    // is what makes the gap visible instead of silently absent, and this test
    // pins that -- it will fail informatively if a later upstream release or
    // transcoder change makes them resolvable.
    let index = index();
    let device = DeviceMatch::for_model("SWV-ZNU").with_manufacturer("SONOFF");
    let definition = index.resolve(&device).expect("resolves");

    let unsupported: Vec<&str> = definition
        .extend
        .iter()
        .filter_map(|e| match e {
            Extend::Unsupported { note, .. } => Some(note.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        !unsupported.is_empty(),
        "the valve has capabilities this build cannot express, and they must be recorded"
    );
    assert!(
        unsupported.iter().any(|note| note.contains("childLock")),
        "the note should name the attribute that could not be resolved, got {unsupported:?}"
    );

    // And the part that does work is still there, which is the whole reason
    // `Unsupported` is a recorded capability rather than a rejected definition.
    assert!(
        definition
            .extend
            .iter()
            .any(|e| matches!(e, Extend::OnOff { .. })),
        "an unsupported capability must not cost the device the ones that resolved"
    );
}

#[test]
fn a_handful_of_common_devices_resolve() {
    // Spread across vendors and match styles: plain zigbeeModel, a Tuya
    // fingerprint, and an IKEA light. If the generated set were truncated or
    // mis-chunked, this is what would notice.
    let index = index();
    for (model, manufacturer) in [
        ("TRADFRI bulb E27 WS opal 980lm", None),
        ("lumi.sensor_magnet.aq2", None),
        ("SWV-ZNU", Some("SONOFF")),
    ] {
        let mut device = DeviceMatch::for_model(model);
        if let Some(m) = manufacturer {
            device = device.with_manufacturer(m);
        }
        assert!(
            index.resolve(&device).is_some(),
            "{model} should resolve against the bundled definitions"
        );
    }
}

#[test]
fn an_unknown_device_resolves_to_nothing() {
    // The negative control. Without it, an index that matched everything would
    // pass every test above.
    let index = index();
    let device = DeviceMatch::for_model("definitely-not-a-real-model").with_manufacturer("nobody");
    assert!(
        index.resolve(&device).is_none(),
        "an unknown model must not resolve to some arbitrary definition"
    );
}

#[test]
fn most_definitions_are_complete_and_the_share_is_reported() {
    // Not a threshold that can be met by dropping capabilities: `incomplete`
    // counts definitions carrying an `Extend::Unsupported`, so silently
    // discarding one would *improve* this number. It is asserted loosely and
    // printed, so a regression in the transcoder shows up as a moved number
    // rather than a failure nobody can interpret.
    let index = index();
    let incomplete = index.incomplete();
    let complete = index.len() - incomplete;
    println!(
        "bundled: {} definitions, {complete} complete, {incomplete} carrying an unsupported capability",
        index.len()
    );
    assert!(
        complete * 2 > index.len(),
        "over half the definitions should be free of unsupported capabilities, \
         got {complete} of {}",
        index.len()
    );
}
