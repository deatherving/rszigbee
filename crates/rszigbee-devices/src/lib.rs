//! Declarative Zigbee device definitions, and the matcher that resolves one for
//! a device.
//!
//! # Why definitions are data and not code
//!
//! A device definition says which cluster and attribute a capability lives on,
//! how to convert its value, and what to configure at join time. Upstream
//! expresses that in TypeScript. Expressing it as data instead means one
//! transcoder can carry thousands of devices across from
//! [zigbee-herdsman-converters], and a new upstream release is a re-run rather
//! than a merge.
//!
//! The built-in definitions are **generated Rust**, not parsed at runtime.
//! There is no deserialiser in the hot path, no runtime parse failure, and an
//! embedded caller links no JSON. Runtime-loaded definitions are available
//! behind the `serde` feature for callers who need them.
//!
//! # What the format has to express, and why exactly these five things
//!
//! Measured against zigbee-herdsman-converters 26.104.0 (4,473 definitions),
//! shared helper references alone top out at **57.9%** of devices no matter how
//! many helpers are implemented — the distribution plateaus. Four more forms
//! carry it to roughly the high seventies:
//!
//! | form | why it is needed |
//! |---|---|
//! | helper references with literal arguments | 57.9% on its own; 64 distinct helpers, the top 20 are 93% of uses |
//! | Tuya datapoint tables | +7%, and 34% of soil-moisture and 44% of illuminance devices are Tuya |
//! | bindings and attribute reporting as a table | 63% of upstream `configure` bodies contain nothing else |
//! | endpoint name maps | 94.8% of upstream `endpoint` bodies are a literal map |
//! | a registered Rust escape hatch | the remaining ~22%, which is genuinely code |
//!
//! The escape hatch is not a failure of the format. Roughly a fifth of the
//! catalogue is real logic — Tuya value converters, IAS enrolment quirks,
//! per-device workarounds — and pretending otherwise would produce a format
//! that silently drops devices. [`Extend::Unsupported`] exists so a transcoder
//! records what it could not express, which is what makes coverage a number
//! that can be watched across upstream releases instead of a guess.
//!
//! # Matching
//!
//! See [`DefinitionIndex`]. The algorithm reproduces upstream's resolution
//! order deliberately, including its subtleties, because a device that resolves
//! to a different definition than it does upstream is a device that behaves
//! differently for no reason a user can see.
//!
//! [zigbee-herdsman-converters]: https://github.com/Koenkk/zigbee-herdsman-converters

#![forbid(unsafe_code)]

mod definition;
mod index;
mod matcher;

pub use definition::{
    Access, Binding, Definition, Extend, NumericSpec, PowerSourceHint, Reporting, TuyaDatapoint,
    TuyaKind, WhiteLabel,
};
pub use index::{DefinitionIndex, IndexError};

/// Identifier types a definition is written in terms of.
///
/// Re-exported so writing a definition by hand does not require depending on
/// `rszigbee-spec` directly.
pub mod reexport {
    pub use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId, ProfileId};
}
pub use matcher::{
    DeviceMatch, EndpointMatch, Fingerprint, FingerprintEndpoint, MatchRules, index_key,
    normalise_model,
};
