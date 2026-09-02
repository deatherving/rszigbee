# rszigbee

A Rust-native Zigbee stack with two first-class modes — an embeddable library
with a typed API, and a Zigbee2MQTT-compatible MQTT gateway — sharing one
runtime, one device model and one compatibility database.

```
                        rszigbee core
                             │
                       typed Rust API
                             │
              ┌──────────────┼──────────────┐
         Embedded API   MQTT adapter   future adapters
              │              │
         Rust apps    Zigbee2MQTT-compatible MQTT
```

> **Status: Phase 1** — architecture and foundational types. Nothing here talks
> to hardware yet. 131 tests, no dongle required.

---

## Contents

- [Why](#why) · [What exists](#what-exists) · [Boundaries](#boundaries)
- Design: [The parse-path invariant](#the-parse-path-invariant) ·
  [Events and commands](#events-and-commands) ·
  [Capabilities vs exposes](#capabilities-vs-exposes) ·
  [Manufacturer-specific clusters](#manufacturer-specific-clusters) ·
  [Reachability](#reachability) · [Persistence](#persistence) ·
  [Backup safety](#backup-safety) · [Device compatibility](#device-compatibility)
- [What is transcoded, not invented](#what-is-transcoded-not-invented) ·
  [Testing](#testing) · [Reliability](#reliability)
- [Plan](#plan) · [Licence](#licence) · [Credit](#credit)

---

## Why

Zigbee2MQTT is excellent and its ecosystem is irreplaceable. Two things it
cannot do: be embedded in a Rust application without an MQTT broker and a
Node.js runtime, and give that application typed events instead of JSON.

The research behind this project produced two measurements that decide the whole
design:

1. **Of the 4,248 device definitions in zigbee-herdsman-converters, 2,400
   (56.5 %) are already pure declarative data** — an `extend:` array with no
   inline JavaScript, no `configure` callback, no arrow function anywhere. A
   further 27.5 % are data plus a little glue; 16 % are legacy imperative
   converters. Only 67 distinct `modernExtend` primitives are used at all, and
   Tuya's entire value-conversion vocabulary is 91 named converters. Porting that
   ecosystem is a data-pipeline problem, not 4,248 rewrites.
2. **Zigbee2MQTT's MQTT surface is a behavioural contract, not a topic naming
   convention.** It is exact down to per-topic retain flags and QoS (`bridge/state`
   is QoS 1 on connect and QoS 0 on graceful disconnect), and down to which of
   its *two* JSON serializers each topic uses — the one that sorts object keys is
   used for device state and every `bridge/*` topic, the insertion-order one for
   `bridge/state`, availability and `bridge/health`. Compatibility means
   reproducing that, and it means proving it with tests rather than claiming it.

## What exists

| Crate | State |
|---|---|
| **`rszigbee-spec`** | ZCL frame codec; ZCL data types including the per-type "invalid" encodings; dynamic cluster registry with per-device custom clusters; ZDO identifiers; address newtypes. Sans-IO. |
| **`rszigbee-adapter`** | The `CoordinatorAdapter` trait, struct-shaped requests, a cancel-safe `Correlator`, and a scriptable `MockAdapter`. |
| **`rszigbee-core`** | Device, capability, state, event, command, reachability and persistence model, plus `MemoryStore`. |
| `spikes/ezsp-probe` | Throwaway read-only probe: does the `ashv2` + `ezsp` crate stack drive real Silicon Labs firmware, and if not, which step fails? |

Not yet built: the runtime task, the Ember adapter, device definitions, the MQTT
compatibility layer, Home Assistant discovery, the converter importer. See
[Plan](#plan).

## Boundaries

Crates exist to enforce a boundary, gate a dependency, or allow independent
publication. Nothing else. `rszigbee-protocol` and `rszigbee-storage` were both
considered and dropped — "protocol primitives" is not a boundary, and one trait
with two implementations does not warrant a crate.

```
rszigbee-spec       sans-IO: no tokio, no serial, no I/O of any kind
rszigbee-adapter    depends on spec; no concrete protocol, no serial
rszigbee-adapter-*  concrete coordinators (ember first)
rszigbee-core       depends on adapter + spec + devices
                    MUST NOT depend on MQTT, JSON, or Home Assistant
rszigbee-devices    depends on spec only
                    MUST NOT depend on core — definitions are data the runtime
                    interprets, not the reverse
rszigbee-mqtt       depends on the facade; one-way
```

`rszigbee-devices` not depending on `rszigbee-core` is the load-bearing rule: it
forces the definition format to be pure data plus pure functions, which is what
makes the importer, the validator and the community contribution story possible.

**These rules are checked, not documented.** `scripts/check-boundaries.sh` runs
in CI and fails the build if the core gains an MQTT or JSON dependency, if the
spec crate gains I/O, if `rszigbee-devices` reaches into the runtime, if
`ZigbeeStore` grows a generic blob method, or if any `unsafe` appears.

## The parse-path invariant

Radio frames, device-reported strings, Tuya datapoints and MQTT payloads are all
**untrusted input**. Every decoder returns `Result` and contains no slice
indexing, `unwrap`, `expect`, `panic!` or overflowing arithmetic. A malformed
frame produces an error; it never takes the process down.

Enforced by four clippy lints denied in CI (`indexing_slicing`, `unwrap_used`,
`expect_used`, `panic`), relaxed inside tests only — asserting on
`decode(..).unwrap()` is how the invariant gets checked. `overflow-checks` stays
on in release: an overflow in a protocol decoder is a bug to report loudly, not a
wrap to rely on. Fuzz targets for every codec follow in Phase 2.

Related: ZCL defines per-type *invalid* encodings (`0xffff` for `uint16`,
`0x8000` for `int16`, NaN for floats) that devices send routinely to mean "no
reading". `ZclValue::Invalid` is a first-class variant, because collapsing it
into an error turns normal traffic into a failure stream, and collapsing it into
zero reports a temperature of 0 °C when the sensor means "I do not know".

## Events and commands

Four deliberate departures from Zigbee2MQTT's internal model:

1. **`Event::StateChanged` carries only the delta.** Publishing the whole merged
   state is an MQTT compatibility behaviour, not an application need. The MQTT
   layer re-applies it; the full snapshot is available on demand.
2. **`Event::Action` is separate from state.** A button press is not state.
   Upstream must fold actions into the state object and then exclude them again
   through a hard-coded nineteen-entry `CACHE_IGNORE_PROPERTIES` list; making the
   distinction structural removes that entire class of bug.
3. **Raw and converted events coexist.** `ZclMessage` and `UnparsedFrame` are
   emitted for every frame, so an unknown device stays useful with no definition
   at all — which is what a user needs in order to contribute one.
4. **`ConverterFailed`, `UnparsedFrame` and `CommandFailed` are events, not log
   lines.** They are the answer to "why is this device not working", and they are
   countable as metrics.

Commands have ergonomic constructors (`SetOn`, `SetBrightness`) that all lower to
one general `Set(StateChanges)` form, so the shortcuts cannot drift from the real
path. `StateChanges` preserves insertion order because command order matters on
real hardware: a bulb that is off may reject a colour change, so `state` moves
relative to `brightness` depending on which way the light is going. That rule is
`StateChanges::prioritise`, tested directly, rather than a comparator buried in a
publish path.

`Confirmation::Queued` is a distinct outcome from `Acked`. It is what makes sleepy
devices honest: without it the only available answers are "success" (a lie) and
"timeout" (also a lie), and callers retry commands already in flight.

## Capabilities vs exposes

Zigbee2MQTT's `exposes` is a real external API — Home Assistant, the
Zigbee2MQTT frontend and countless scripts parse it — and must be reproduced
exactly. But it is shaped by JSON: `access` is a raw 3-bit mask, units are free
strings, and the endpoint is concatenated into the property name (`state_left`).

The internal `Capability` model uses a typed `Unit`, named `Access` flags,
`EndpointId` as a separate field, and a `CapabilityKind::Action`. `rszigbee-mqtt`
owns the mapper to `exposes` and re-applies the suffixing there, so the
compatibility ugliness is confined to one function in one crate.

`Capability::accepts` validates against the declared domain in the core, so a
definition's declared range is a safety boundary rather than documentation — an
MQTT payload cannot push a device past it.

## Manufacturer-specific clusters

Not an edge case: `deviceAddCustomCluster` is called **388 times** across
zigbee-herdsman-converters, making it the third most common thing a device
definition does. So the cluster registry is data, with per-device overrides:

```rust
registry.insert_for_device(ieee, ClusterDef::new(0xfc03, "manuSpecificPhilips2")
    .cmd(0x00, "multiColor", &[("data", ZclType::OctStr)]));
```

Lookups fall back device-specific → global, so a device can both add clusters and
override attributes on a standard one, which real devices do. Custom clusters are
persisted with the device record, so decoding works on restart before the
definition has been resolved.

A typed-module-per-cluster design cannot express "this particular device has
cluster `0xfc03` whose attribute 3 is a `uint16`". That is a representation
difference, not a coverage gap, and it is why rszigbee builds its own ZCL layer
rather than adopting an existing typed one.

## Reachability

Split by fact versus policy, so an embedded application does not need MQTT to
know whether a device is answering.

**Core owns the facts** (`last_seen`, `last_tx_ok`, `last_tx_err`, `last_probe`,
`consecutive_probe_failures`, `is_sleepy`) **and the mechanism** — one timer, one
serialized probe queue. One scheduler, because two would fight over the radio and
produce ping storms.

**Policy is injected** and decides *when*:

```rust
trait ReachabilityPolicy { fn assess(&self, ctx: &ReachabilityContext) -> Assessment; }
enum NextCheck { Probe { at, attempts, allow_recovery }, Reassess { at }, AwaitTraffic }
```

The vocabulary is deliberately domain-neutral — no `timeout`, which conflates
"how long until unreachable" with "how long until we probe" and presumes a
timeout exists at all. That the three variants suffice is the test of whether the
seam is in the right place:

| Zigbee2MQTT behaviour | variant |
|---|---|
| active device, ping after the 10-minute timeout | `Probe { attempts: 2, allow_recovery }` |
| passive device, offline after 1500 minutes, never ping | `Reassess` |
| `pause_on_backoff_gt` reached: stop until traffic arrives | `AwaitTraffic` |

Backoff, jitter and hysteresis are policy-internal; core never sees them.
`Reachability::Unknown` is distinct from `Unreachable`, because reporting a
freshly restarted device as offline before it has spoken produces a wave of false
notifications on every restart.

## Persistence

`ZigbeeStore` holds network identity, devices, groups and coordinator backups —
**Zigbee domain state and nothing else.** An earlier draft had generic
`get_blob`/`put_blob` methods as somewhere for the MQTT layer to keep its name
registry; that violated this project's own "MQTT must not leak into core" rule
and would have become a dumping ground with no schema, no versioning and no
owner. Layers above core own their own persistence.

Deliberately **not** built: a generic `KeyValueStore` both could share. It is the
obvious eventual answer for single-backend ergonomics and also exactly the
premature abstraction the crate rules exist to prevent. Add it when a user asks.

Per-device writes, not whole-file rewrites: upstream rewrites its entire database
on any change, which at a thousand devices is a multi-megabyte fsync every time a
device is heard from.

`PersistedDevice::passthrough` preserves unrecognised imported fields verbatim,
which is what makes a Zigbee2MQTT import lossless and a rollback possible, and
stops an older rszigbee destroying a field written by a newer one.

Corruption is handled by *what* was corrupt: a corrupt state cache is
quarantined and startup continues; corrupt **network identity** stops startup,
because continuing means forming a new network and orphaning every device.

## Backup safety

The most destructive thing this project can do is form a network when it should
have resumed one, or roll a frame counter backwards. So:

- **`MismatchPolicy::Fail` is the default.** If the coordinator holds a network
  that does not match the configuration, startup stops and says why. Forming is
  opt-in.
- **Frame counters only move forward.** On restore, the counter written is
  `max(backup, coordinator) + margin`; a coordinator ahead of its backup needs an
  explicit force flag.
- **Validate before writing.** Format, version, stack family and coordinator
  IEEE are checked; an EZSP backup onto a Z-Stack dongle fails loudly rather than
  partially succeeding.
- **Snapshot before restore**, always, and keep versioned history rather than one
  overwritten file.
- Format is `zigpy/open-coordinator-backup` v1, read and written compatibly — a
  real cross-project interchange format shared with the zigpy/ZHA ecosystem.

## Device compatibility

Three converter tiers, sized to the measured population:

| Tier | Covers | Form |
|---|---|---|
| **Declarative** | the top-20 `modernExtend` primitives and all 348 Tuya datapoint tables | pure data, interpreted by a fixed engine |
| **Named** | anything upstream expresses as a reference to a shared function (~40 `fz.*`, ~40 `tz.*`, 91 Tuya converters, ~30 reporting recipes) | a name plus args in the data; one Rust implementation each |
| **Rust** | arrow functions, stateful click counters, dynamic endpoint generation, the 62 function-valued `exposes`, the 26 `onEvent` | a registered `DeviceBehaviour` impl |

Definitions are TOML, one file per model, directories by vendor. **The schema is
experimental (`schema = 0`) and will not be frozen** until at least ten
materially different device classes are expressed in it and the importer has
lowered a stratified sample of 300+ real definitions without needing a change.

The importer extracts by **executing** upstream, not by parsing TypeScript:
arguments there are frequently computed or shared, and `configure` is traced
against an instrumented mock rather than read. Its output is the *specification
for* the definition format, not a validation of it — designing the format from
conceptual examples and discovering later that real definitions do not fit is the
most expensive mistake available. Synchronisation is a weekly CI job that opens
one reviewable PR with a coverage diff and a regression gate.

No JavaScript runtime, ever. No embedded QuickJS to run upstream converters
unmodified: it would import the entire attack surface and performance profile
rszigbee exists to avoid, and make the type system decorative.

## What is transcoded, not invented

Data is transcoded from MIT-licensed upstream with attribution; implementations
are written fresh. Retyping curated data by hand would introduce errors and take
weeks.

| Transcoded as data | Source |
|---|---|
| ZCL clusters, attributes, commands (129 clusters) | `zigbee-herdsman/src/zspec/zcl/definition/cluster.ts` |
| ZCL data types, foundation commands | `.../definition/{datatypes,foundation}.ts` |
| Manufacturer codes (723 entries) | `.../definition/manufacturerCode.ts` |
| ZDO cluster ids and response shapes | `zigbee-herdsman/src/zspec/zdo/definition/*` |
| Interview quirk table | `zigbee-herdsman/src/controller/model/device.ts` |
| USB adapter fingerprints | `zigbee-herdsman/src/adapter/adapterDiscovery.ts` |
| Device definitions | `zigbee-herdsman-converters/src/devices/*` |

Depended on rather than reimplemented: `ashv2` and `ezsp` (ASH v2 framing and the
EZSP protocol — herdsman's equivalents are 1,933 and 9,110 lines of subtle logic
with no differentiating value here), RustCrypto for AES-CCM\*, `rumqttc` for MQTT.

The interview quirk table deserves a specific note: it is accumulated field
knowledge that cannot be re-derived from a specification. Without it, roughly
half of any real test bench mysteriously fails to pair.

## Testing

`cargo test --workspace` must pass with **no dongle, no broker, no Node.js**.
That is a hard requirement, not an aspiration.

- `MockAdapter` is compiled unconditionally and is the reference implementation
  of the adapter trait. If something is awkward to express there, the trait is
  probably wrong.
- Recorded serial fixtures replay real ASH/EZSP sessions through the real
  adapter, so the Ember path is tested without hardware. A `--capture` mode turns
  every hardware bug report into a permanent regression test — the single
  highest-return testing investment available.
- Converter fixtures are largely auto-generated by the importer, so coverage
  scales with definition count rather than with hand-written tests.
- MQTT compatibility is verified against golden fixtures harvested from
  upstream's own test suite, compared at five layers: topic set, retain/QoS
  flags, payload semantics, payload bytes, and ordering where it is semantically
  relevant.
- **Nothing is marked compatible without a passing test id.** `COMPATIBILITY.md`
  is generated from test results, not hand-maintained.

## Reliability

Handled by design, not by hope: serial disconnect and reconnect, coordinator
firmware reset, coordinator IEEE change (refused without an explicit flag),
process restart mid-interview (`InProgress` persists as `Pending` so it resumes),
device rejoin and address change, sleepy-device command buffering, duplicate
frame suppression, malformed input at every layer, corrupt persistence, and
bounded channels everywhere so a slow consumer drops with a counter instead of
growing without limit.

Shutdown is layered, and the `CoordinatorAdapter` never knows MQTT exists: the
gateway stops the MQTT layer (which publishes the retained offline state), then
the Zigbee runtime, then the adapter.

## Plan

| Phase | Content |
|---|---|
| 0 | Research ✅ |
| **1** | **Architecture, foundational types, CI, ADRs. Also: harvest the MQTT golden corpus and build the upstream extractor — both are specification inputs, so they come before the code they constrain.** |
| 2 | Ember vertical slice: serial → network → permit join → join → interview → typed event, and a typed command back out. One smart plug, no MQTT. |
| 3 | Six representative devices; sleepy-device queue; reachability policy |
| 4 | Stabilise the embedded API; publish `0.1.0` |
| 5 | MQTT compatibility: state, `/set`, `/get`, `bridge/*`, availability, retain |
| 6 | Device compatibility framework; freeze the definition schema |
| 7 | Full converter importer and weekly upstream sync |
| 8 | Home Assistant discovery |
| 9 | Groups, binding, reporting, backup/restore, migration, Z-Stack, OTA |

**Explicit non-goals:** no JavaScript runtime, no web frontend (make `bridge/*`
good enough for the existing Zigbee2MQTT frontend instead), no MQTT or Home
Assistant dependency in the core, no invented cryptography, no cloud or
telemetry, no non-Zigbee protocols, no Zigbee certification claims, and no
performance claims as a selling point. Correctness, compatibility, reliability,
maintainability, observability — then performance.

## Development

```sh
cargo test --workspace                                              # no hardware
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
./scripts/check-boundaries.sh                                       # architecture rules

# hardware spike, read-only and safe against a live network:
cd spikes/ezsp-probe && cargo run -- /dev/cu.usbserial-XXXX
```

Decisions are recorded as ADRs. They are kept out of this repository for now
along with the full research report; the reasoning that the code depends on has
been consolidated into this file.

## Licence

Intended: `MIT OR Apache-2.0` across the whole workspace, including the MQTT
compatibility layer, so that embedders, vendors and downstream Rust projects can
all use every crate.

**This is not final.** zigbee-herdsman and zigbee-herdsman-converters are MIT, so
transforming their data is permitted with attribution. Zigbee2MQTT is
**GPL-3.0**, and it defines the MQTT contract the compatibility layer targets.
The position is that reproducing an interface from observed behaviour is not the
same as translating an implementation — so nobody implementing `rszigbee-mqtt`
works from Zigbee2MQTT source, the Home Assistant property tables are rebuilt
from Home Assistant's own documentation rather than extracted, and harvested test
fixtures stay test-only and excluded from published crates.

That position needs review by a lawyer who knows OSS licensing before release.
If it does not hold, the fallback is `GPL-3.0-or-later` for `rszigbee-mqtt` and
`rszigbee-cli` with the core staying permissive; the workspace is laid out so
that costs one `Cargo.toml` field per crate and no code movement.

## Credit

rszigbee exists because of work other people did first.

- **[zigbee-herdsman](https://github.com/Koenkk/zigbee-herdsman)** (MIT) — the
  ZCL and ZDO data, the adapter boundary, and the interview quirks no
  specification will tell you about.
- **[zigbee-herdsman-converters](https://github.com/Koenkk/zigbee-herdsman-converters)**
  (MIT) — compatibility knowledge for thousands of devices, contributed by
  hundreds of people who each bought hardware and worked out how it behaves.
  rszigbee's device data originates here. **Device fixes belong upstream**, where
  the whole ecosystem benefits, not only in this fork of the data.
- **[Zigbee2MQTT](https://github.com/Koenkk/zigbee2mqtt)** (GPL-3.0) — the MQTT
  contract treated here as an external API to reproduce.
- **[zigpy/ziggurat](https://github.com/zigpy/ziggurat)** (Apache-2.0) and
  **[apis-saltans](https://github.com/PaulmannLighting/apis-saltans)** (MIT) —
  both informed the architecture. apis-saltans independently converged on almost
  the same coordinator-adapter boundary, which is decent evidence it is the right
  one. Adapter support and trait alignment are on the roadmap, as conversations
  rather than forks.
- **[uplg/maison](https://github.com/uplg/maison)** (MIT) — proved the
  `ezsp` + `ashv2` stack drives real Silicon Labs hardware from Rust.

See [`ATTRIBUTION.md`](ATTRIBUTION.md) and
[`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).
