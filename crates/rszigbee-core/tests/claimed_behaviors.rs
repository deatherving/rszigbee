//! Checks that every named behaviour the transcoder delegates to exists.
//!
//! A definition naming a behaviour nothing implements is not a crash and not a
//! fallback — the datapoint is simply unhandled. That is the right runtime
//! behaviour, and it is also exactly why the *claim* has to be checked
//! somewhere: without this, adding a name to the transcoder would move
//! definitions into the "handled by Rust" column while nothing handled them.
//!
//! The list is generated; regenerate with `scripts/refresh-device-coverage.sh`.

#![allow(clippy::expect_used, clippy::panic)]

use rszigbee_core::runtime::BehaviorRegistry;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Claims {
    /// Behaviour names the transcoder delegates datapoints to.
    #[serde(default)]
    behaviors: Vec<String>,
}

#[test]
fn every_behaviour_the_transcoder_delegates_to_is_shipped() {
    let claims: Claims = serde_json::from_str(include_str!(
        "../../rszigbee-devices/tests/fixtures/claimed-primitives.json"
    ))
    .expect("claimed-primitives.json should parse");

    assert!(
        !claims.behaviors.is_empty(),
        "the transcoder delegates at least one converter, so the list must be published"
    );

    let registry = BehaviorRegistry::with_builtins();
    let missing: Vec<&String> = claims
        .behaviors
        .iter()
        .filter(|name| registry.get(name).is_none())
        .collect();

    assert!(
        missing.is_empty(),
        "the transcoder delegates to behaviours this build does not ship: {missing:?}. \
         Either implement them or stop delegating — a name with no implementation \
         moves devices into the 'handled by Rust' column while nothing handles them. \
         Shipped: {:?}",
        registry.names().collect::<Vec<_>>()
    );
}

#[test]
fn a_name_nothing_implements_is_absent_rather_than_defaulted() {
    // The runtime relies on this: an unimplemented behaviour means the
    // datapoint goes unhandled, not that some default interprets it.
    let registry = BehaviorRegistry::with_builtins();
    assert!(registry.get("nobody:implements-this").is_none());
}
