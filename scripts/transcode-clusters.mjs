// Transcodes zigbee-herdsman's ZCL cluster table into generated Rust.
//
// `crates/rszigbee-spec/src/zcl/builtin.rs` used to be a hand-written subset of
// seven clusters, and its own documentation said the full table wanted
// transcoding rather than typing. This is that: 129 clusters with their
// attributes, commands and command responses, read from herdsman's *runtime*
// objects rather than parsed out of its source, so the numbers are the ones it
// actually uses.
//
// Why it matters beyond tidiness: without the table, a plan step has to carry
// its own wire type, an attribute report has to be decoded by the type on the
// wire with nothing to check it against, and a cluster name cannot be resolved
// to an id at all. All three were real limitations.
//
// zigbee-herdsman is MIT, (c) 2019 Jack Wu, Simen Li, Hedy Wang and Koen
// Kanters. This reads its data and emits our own representation; no code is
// copied or translated.
//
//   node scripts/transcode-clusters.mjs > crates/rszigbee-spec/src/zcl/generated.rs

import {Clusters} from 'zigbee-herdsman/dist/zspec/zcl/definition/cluster.js';

/** Rust string literal, for names that are not plain identifiers. */
function str(value) {
  return `"${String(value).replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

/**
 * Renders one attribute as a tuple.
 *
 * The trailing manufacturer code is 0 when there is none. Zero is not a valid
 * manufacturer code, so it doubles as the absent marker and keeps the table a
 * flat tuple rather than nesting an Option in 1,354 rows.
 *
 * As of the version transcoded here upstream carries *no* per-attribute codes
 * -- only one cluster-level one, on `manuSpecificAmazonWWAH` -- so this field
 * is currently always zero. It is read anyway so that a future release adding
 * one is carried through rather than silently dropped.
 */
function attr(name, def) {
  const manufacturer = def.manufacturerCode ?? 0;
  return `(${def.ID}, ${str(name)}, ${def.type}, ${manufacturer})`;
}

/**
 * Renders one command, with its ordered parameters.
 *
 * Upstream uses synthetic type codes above 255 for composite and list-valued
 * parameters -- scene extension field sets, group lists -- which our `ZclType`
 * has no representation for. Such a command is emitted with a fourth field set
 * so the Rust side marks it unencodable: it is still worth knowing by name for
 * identifying a received frame, but encoding it with an empty payload would
 * produce a frame that is silently too short.
 */
function cmd(name, def) {
  const params = def.parameters ?? [];
  const untypeable = params.some((p) => p.type > 255);
  if (untypeable) {
    return `(${def.ID}, ${str(name)}, &[], true)`;
  }
  const rendered = params.map((p) => `(${str(p.name)}, ${p.type})`).join(', ');
  return `(${def.ID}, ${str(name)}, &[${rendered}], false)`;
}

const rows = [];
let attributes = 0;
let commands = 0;

for (const [name, cluster] of Object.entries(Clusters).sort((a, b) => a[1].ID - b[1].ID)) {
  const attrs = Object.entries(cluster.attributes ?? {}).map(([n, d]) => attr(n, d));
  const cmds = Object.entries(cluster.commands ?? {}).map(([n, d]) => cmd(n, d));
  const rsps = Object.entries(cluster.commandsResponse ?? {}).map(([n, d]) => cmd(n, d));
  attributes += attrs.length;
  commands += cmds.length + rsps.length;

  rows.push(`    Spec {
        id: ${cluster.ID},
        name: ${str(name)},
        manufacturer: ${cluster.manufacturerCode ?? 0},
        attrs: &[${attrs.length ? `\n            ${attrs.join(',\n            ')},\n        ` : ''}],
        cmds: &[${cmds.length ? `\n            ${cmds.join(',\n            ')},\n        ` : ''}],
        rsps: &[${rsps.length ? `\n            ${rsps.join(',\n            ')},\n        ` : ''}],
    }`);
}

process.stdout.write(`//! The ZCL cluster table, generated from zigbee-herdsman.
//!
//! **Do not edit.** Regenerate with \`scripts/refresh-clusters.sh\`.
//!
//! ${rows.length} clusters, ${attributes} attributes, ${commands} commands and
//! command responses, transcoded from zigbee-herdsman's own runtime
//! definitions (MIT, (c) 2019 Jack Wu, Simen Li, Hedy Wang and Koen Kanters).
//!
//! Names deliberately keep upstream's spelling — \`genOnOff\`, not \`on_off\` —
//! because those names appear in imported device definitions, in diagnostics
//! and in every community discussion of Zigbee devices. Renaming them would
//! make the ecosystem's accumulated knowledge stop applying.
//!
//! Stored as a flat static table and turned into [\`ClusterDef\`] values on
//! demand, rather than as ${rows.length} builder functions: the table costs
//! almost nothing to compile, and the allocation happens once when a registry
//! is built.

use alloc::vec::Vec;

use crate::ids::ManufacturerCode;
use crate::zcl::registry::ClusterDef;
use crate::zcl::types::ZclType;

/// \`(id, name, wire type tag, manufacturer code)\`.
///
/// A manufacturer code of zero means none: zero is not a valid code, so it
/// serves as the absent marker without nesting an \`Option\` in every row.
type Attr = (u16, &'static str, u8, u16);

/// \`(name, wire type tag)\`.
type Param = (&'static str, u8);

/// \`(id, name, ordered parameters, parameters untypeable)\`.
type Cmd = (u8, &'static str, &'static [Param], bool);

/// One cluster, as static data.
struct Spec {
    id: u16,
    name: &'static str,
    /// Zero when the cluster is not manufacturer specific.
    manufacturer: u16,
    attrs: &'static [Attr],
    /// Client-to-server commands.
    cmds: &'static [Cmd],
    /// Server-to-client responses.
    rsps: &'static [Cmd],
}

/// How many clusters this build knows.
pub const COUNT: usize = ${rows.length};

/// The clusters this build ships.
#[must_use]
pub fn clusters() -> Vec<ClusterDef> {
    SPECS.iter().map(build).collect()
}

/// Turns one static spec into a [\`ClusterDef\`].
fn build(spec: &Spec) -> ClusterDef {
    let mut def = ClusterDef::new(spec.id, spec.name);
    if spec.manufacturer != 0 {
        def.manufacturer = Some(ManufacturerCode(spec.manufacturer));
    }
    for &(id, name, tag, manufacturer) in spec.attrs {
        def = def.attr(id, name, ZclType::from_u8(tag));
        if manufacturer != 0 {
            // Set after the fact because the builder does not take it: a
            // manufacturer-specific attribute is only readable when the code
            // is sent with the request, and losing that makes the read fail.
            if let Some(entry) = def.attributes.get_mut(&id) {
                entry.manufacturer = Some(ManufacturerCode(manufacturer));
            }
        }
    }
    for &(id, name, params, untyped) in spec.cmds {
        def = if untyped {
            def.cmd_untyped(id, name)
        } else {
            def.cmd(id, name, &to_params(params))
        };
    }
    for &(id, name, params, untyped) in spec.rsps {
        def = if untyped {
            def.rsp_untyped(id, name)
        } else {
            def.rsp(id, name, &to_params(params))
        };
    }
    def
}

/// Resolves parameter type tags.
fn to_params(params: &'static [Param]) -> Vec<(&'static str, ZclType)> {
    params
        .iter()
        .map(|&(name, tag)| (name, ZclType::from_u8(tag)))
        .collect()
}

static SPECS: &[Spec] = &[
${rows.join(',\n')},
];
`);

process.stderr.write(
  `clusters: ${rows.length}, attributes: ${attributes}, commands: ${commands}\n`,
);
