# Attribution

rszigbee is a reimplementation, not an original invention. This file states
plainly what it owes to whom.

## zigbee-herdsman-converters (MIT)

The single largest debt. Device compatibility for thousands of Zigbee devices —
fingerprints, attribute conversions, manufacturer quirks, Tuya datapoint tables,
the knowledge that a particular sensor reports temperature in an unusual scale —
represents years of work by hundreds of contributors who each bought a device and
worked out how it behaves.

rszigbee's device definitions are **derived from that data** by an automated
importer. Every generated definition file carries a provenance header naming the
upstream version and source. This is permitted by the MIT licence with
attribution, and attribution is the least of what is owed.

**Resolution order.** Which definition claims a given device is decided by
upstream's algorithm, reimplemented in `rszigbee-devices` from its documented
behaviour rather than translated. `crates/rszigbee-devices/tests/fixtures/`
holds test-only data harvested by *running* upstream's resolver over its own
catalogue, so that our answers can be checked against its answers rather than
against our reading of its source. Regenerate with
`scripts/refresh-device-fixtures.sh`.

Matching upstream exactly is not deference for its own sake: a device that
resolves here to a different definition than it does there behaves differently
for no reason its owner could see.

**Practical commitment:** where rszigbee's differential testing finds a genuine
bug in an upstream definition — and it will, because differential testing is good
at that — the fix goes upstream, not only into rszigbee. Quietly harvesting a
volunteer community's work while contributing nothing back would be both bad
citizenship and strategically fragile.

## zigbee-herdsman (MIT)

- The ZCL cluster, attribute, command and data-type tables, and the ZDO cluster
  definitions, transcoded as data. `crates/rszigbee-spec/src/zcl/generated.rs`
  is generated from herdsman's own runtime definitions by
  `scripts/refresh-clusters.sh`: 129 clusters, 1,354 attributes, 424 commands.
  Cluster and attribute names keep upstream's spelling deliberately, because
  those names are what the ecosystem's accumulated knowledge is written in.
- The `Adapter` boundary, which has survived six adapter families and is the
  right place to cut. `CoordinatorAdapter` is a Rust rendering of it.
- The interview sequence and its quirk registry. This is field knowledge that
  cannot be re-derived from a specification: without it, roughly half of any real
  test bench mysteriously fails to pair.
- The `zigpy/open-coordinator-backup` interchange format, shared with the
  zigpy/ZHA ecosystem.

## Zigbee2MQTT (GPL-3.0)

Zigbee2MQTT defines the MQTT contract that rszigbee's compatibility mode targets:
topic structures, payload shapes, retain and QoS behaviour, the `bridge/*`
request/response API, availability semantics and Home Assistant discovery.

rszigbee treats that contract as an external API to be reproduced from observed
behaviour, and does **not** translate Zigbee2MQTT's implementation. See the licence section of the README for the reasoning and the clean-room
discipline that goes with it.

## zigpy/Ziggurat (Apache-2.0) and apis-saltans (MIT)

Not dependencies today, but both informed the architecture. Ziggurat
demonstrated that a host-side Zigbee stack over an RCP radio is viable in Rust;
apis-saltans independently converged on almost the same coordinator-adapter
boundary, which is decent evidence the boundary is right. A future adapter for
Ziggurat and trait alignment with apis-saltans are both on the roadmap, and both
are better pursued as conversations than as forks.

## uplg/maison (MIT)

Proved that the `ezsp` + `ashv2` crate stack drives real Silicon Labs hardware
from Rust. Used as a working reference for the EZSP bring-up sequence — the part
the specification does not tell you — not as a source of code.
