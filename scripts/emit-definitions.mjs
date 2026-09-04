// Turns the transcoder's IR into generated Rust device definitions.
//
// `transcode-devices.mjs` decides *what* each upstream definition means and
// writes `definitions.json`. This turns that into Rust the crate ships, which
// is the step that was missing: the coverage report claimed 48.9% usable while
// `DefinitionIndex::new()` -- an empty index -- was what callers actually got.
// A number describing what a format could express is not the same as a device
// that works, and the gap was invisible because everything ran against a mock.
//
// Two resolutions happen here and nowhere else, because the IR carries names
// where Rust needs numbers:
//
//   cluster/attribute names   resolved against zigbee-herdsman's own table, so
//                             the ids agree with `zcl/generated.rs` by
//                             construction rather than by review
//   endpoint names            resolved against the same definition's own
//                             endpoint map, since `endpointNames: ["left"]`
//                             only means anything next to `{left: 1}`
//
// Anything that does not resolve becomes `Extend::Unsupported`, naming what was
// missing. That is deliberate: dropping it would silently promote a broken
// definition to a working one, which is exactly the failure this file exists to
// close.
//
// zigbee-herdsman and zigbee-herdsman-converters are MIT, (c) Koen Kanters and
// contributors. This reads their data and emits our own representation.
//
//   node scripts/emit-definitions.mjs definitions.json match-rules.json \
//     > crates/rszigbee-devices/src/generated.rs

import fs from 'node:fs';
import {Zcl} from 'zigbee-herdsman';

const [irPath, matchPath] = process.argv.slice(2);
if (!irPath || !matchPath) {
  process.stderr.write('usage: emit-definitions.mjs definitions.json match-rules.json\n');
  process.exit(2);
}

const irs = JSON.parse(fs.readFileSync(irPath, 'utf8'));
const matchRules = JSON.parse(fs.readFileSync(matchPath, 'utf8'));

/**
 * Match rules, keyed by model.
 *
 * These come from the harvest rather than from the IR on purpose. The IR's
 * `match` is read out of upstream's *source*, and that extraction fails for 720
 * definitions whose rules are not literals a parser can see. `match-rules.json`
 * is read out of upstream's *runtime*, is complete, and is the file the
 * differential test checks 4473/4473 against. Using the IR's copy silently lost
 * 16% of the catalogue to `IndexError::Unreachable`.
 */
const rulesByModel = new Map(matchRules.map((rule) => [rule.m, rule]));

/**
 * Custom cluster ids, harvested from every `AddCustomCluster` in the corpus.
 *
 * Upstream declares a manufacturer cluster in one shared module and then
 * *names* it from many definitions -- `customClusterEwelink` is 0xFC11 and is
 * referenced by 135 capabilities whose own definition never declares it. With
 * only zigbee-herdsman's standard table those references cannot resolve at all.
 *
 * A name that appears with two different ids is dropped rather than guessed: a
 * wrong cluster id does not fail loudly, it binds to whatever cluster happens
 * to live at that number and the device then reports nothing while looking
 * configured.
 */
const customClusters = (() => {
  const seen = new Map();
  const conflicting = new Set();
  // Attribute names too: a custom cluster's attributes are not in any standard
  // table either, so `childLock` on `customClusterEwelink` can only be resolved
  // from the declaration that introduced it.
  const attributes = new Map();
  for (const ir of irs) {
    for (const entry of ir.extend ?? []) {
      if (entry.helper !== 'AddCustomCluster') continue;
      const {name, id} = entry.args ?? {};
      if (typeof name !== 'string' || typeof id !== 'number') continue;
      const previous = seen.get(name);
      if (previous !== undefined && previous !== id) conflicting.add(name);
      else seen.set(name, id);
      let byName = attributes.get(name);
      if (!byName) attributes.set(name, (byName = new Map()));
      for (const attribute of entry.args.attributes ?? []) {
        const [attrId, attrName] = attribute;
        if (typeof attrId === 'number' && typeof attrName === 'string') {
          byName.set(attrName, attrId);
        }
      }
    }
  }
  for (const name of conflicting) seen.delete(name);
  return {map: seen, conflicting, attributes};
})();

const stats = {
  definitions: 0,
  extends: 0,
  unsupported: 0,
  fractionalRanges: 0,
  reportings: 0,
  unreachable: 0,
  clusterMisses: new Map(),
  attributeMisses: new Map(),
};

function note(map, key) {
  map.set(key, (map.get(key) ?? 0) + 1);
}

// ---------------------------------------------------------------- primitives

/** A Rust string literal. */
function str(value) {
  return `"${String(value).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n')}"`;
}

/**
 * An owned `String`, as `String::new()` when empty.
 *
 * `"".into()` is what clippy calls creating an empty String manually, and a
 * few hundred definitions carry no vendor or description.
 */
function owned(value) {
  const text = String(value ?? '');
  return text === '' ? 'String::new()' : `${str(text)}.into()`;
}

/** `Some(x)` / `None`. */
function opt(value) {
  return value === null || value === undefined ? 'None' : `Some(${value})`;
}

/** A `Vec<T>` from already-rendered elements. */
function vec(items) {
  return items.length ? `vec![${items.join(', ')}]` : 'Vec::new()';
}

// ------------------------------------------------------------- id resolution

/** Cluster name to id, or null when this build cannot resolve it. */
function clusterId(name) {
  if (typeof name === 'number') return name;
  const standard = Zcl.Clusters[name];
  if (standard) return standard.ID;
  const custom = customClusters.map.get(name);
  if (custom !== undefined) return custom;
  note(stats.clusterMisses, customClusters.conflicting.has(name) ? `${name} (ambiguous id)` : name);
  return null;
}

/**
 * Attribute name to id *within a named cluster*.
 *
 * Both must resolve. An attribute id looked up in the wrong cluster is not a
 * failure that shows up as an error; it produces a read of whatever attribute
 * happens to live at that number, which returns a plausible value.
 */
function attributeId(clusterName, attributeName) {
  if (typeof attributeName === 'number') return attributeName;
  // A manufacturer-specific attribute is declared inline with its own id and
  // wire type rather than named, because it is not in any standard table.
  if (attributeName && typeof attributeName === 'object' && typeof attributeName.ID === 'number') {
    return attributeName.ID;
  }
  const cluster = Zcl.Clusters[clusterName];
  const standard = cluster?.attributes?.[attributeName];
  if (standard) return standard.ID;
  const custom = customClusters.attributes.get(clusterName)?.get(attributeName);
  if (custom !== undefined) return custom;
  note(stats.attributeMisses, `${clusterName}.${attributeName}`);
  return null;
}

/**
 * Endpoint names to ids, using this definition's own endpoint map.
 *
 * Returns null if any name is unknown, because a partially resolved endpoint
 * list is a definition that acts on some of the device's gangs and silently
 * ignores the rest.
 */
function endpointIds(names, endpointMap) {
  if (!names?.length) return [];
  const out = [];
  for (const name of names) {
    const id = endpointMap.get(String(name));
    if (id === undefined) return null;
    out.push(id);
  }
  return out;
}

/** Upstream's access strings, mapped onto `Access`. */
function access(value) {
  switch (value) {
    case 'ALL':
    case 'STATE_SET':
      return 'Access::ReportAndSet';
    case 'SET':
      return 'Access::Set';
    default:
      // STATE, STATE_GET, and anything unrecognised: readable but not
      // writable. The conservative direction -- a capability wrongly marked
      // writable invites a command the device will refuse.
      return 'Access::Report';
  }
}

/**
 * `NumericSpec`, emitting only what differs from the default.
 *
 * Returns `{why}` instead of a spec when the conversion cannot be represented.
 * That case must not fall back to emitting the capability without its divisor:
 * a temperature whose divisor is dropped does not read as unsupported, it
 * reads as 2137 degrees.
 */
function numericSpec(args) {
  const fields = [];
  const divisor = args.scale ?? args.divisor;
  if (divisor !== undefined && divisor !== null && !Number.isInteger(divisor)) {
    return {why: `a divisor of ${divisor} is not an integer, so the conversion cannot be represented`};
  }
  if (divisor && divisor !== 1) fields.push(`divisor: ${divisor}`);
  if (args.offset) fields.push(`offset: ${args.offset}`);
  if (args.unit) fields.push(`unit: Some(${owned(args.unit)})`);
  if (args.valueMin !== undefined && args.valueMax !== undefined) {
    // `range` is in converted units and integral, but upstream's bounds are
    // display values and can be fractional -- 0.1 to 40 degrees. Rounding
    // would move a limit the device declared, and scaling by the divisor
    // would mix raw and converted units, so a fractional bound is dropped
    // and counted. The capability itself is unaffected; only the hint is.
    if (Number.isInteger(args.valueMin) && Number.isInteger(args.valueMax)) {
      fields.push(`range: Some((${args.valueMin}, ${args.valueMax}))`);
    } else {
      stats.fractionalRanges += 1;
    }
  }
  if (!fields.length) return {spec: 'NumericSpec::default()'};
  return {spec: `NumericSpec { ${fields.join(', ')}, ..Default::default() }`};
}

// ------------------------------------------------------------------- extends

/** An `Extend::Unsupported`, recording why. */
function unsupported(helper, why) {
  stats.unsupported += 1;
  return `Extend::Unsupported { helper: ${owned(helper)}, note: ${owned(why)} }`;
}

/**
 * One IR helper as an `Extend`.
 *
 * Returns `Extend::Unsupported` rather than throwing or skipping: the point of
 * that variant is that a definition records what it could not express, so
 * coverage stays measurable across upstream releases.
 */
function extend(entry, endpointMap) {
  const {helper} = entry;
  const args = entry.args ?? {};

  /** Shared by the several helpers that are a plain numeric attribute. */
  const numericHelper = (name) => {
    const spec = numericSpec(args);
    return spec.why ? unsupported(helper, spec.why) : `Extend::${name}(${spec.spec})`;
  };

  /** Shared by the helpers that carry a cluster + attribute pair. */
  const resolved = () => {
    const cluster = clusterId(args.cluster);
    if (cluster === null) return {why: `cluster ${args.cluster} is not in this build's registry`};
    const attribute = attributeId(args.cluster, args.attribute);
    if (attribute === null) {
      return {why: `attribute ${args.attribute} is not known on cluster ${args.cluster}`};
    }
    return {cluster, attribute};
  };

  switch (helper) {
    case 'Light': {
      // Brightness is implied: upstream's light helper always exposes it.
      const range = args.colorTemp?.range;
      const colorTemp = Array.isArray(range) ? `Some((${range[0]}, ${range[1]}))` : 'None';
      return `Extend::Light { brightness: true, color_temp: ${colorTemp}, color: ${!!args.color} }`;
    }
    case 'Identify':
      return 'Extend::Identify';
    case 'OnOff': {
      const endpoints = endpointIds(args.endpointNames, endpointMap);
      if (endpoints === null) {
        return unsupported(helper, `endpoint names ${JSON.stringify(args.endpointNames)} are not in the endpoint map`);
      }
      return `Extend::OnOff { endpoints: ${vec(endpoints.map((e) => `EndpointId(${e})`))}, power_on_behavior: ${!!args.powerOnBehavior} }`;
    }
    case 'Battery':
      return `Extend::Battery { voltage: ${!!args.voltage} }`;
    case 'DeviceEndpoints':
      return 'Extend::DeviceEndpoints';
    case 'ElectricityMeter':
      return 'Extend::ElectricityMeter';
    case 'Temperature':
      return numericHelper('Temperature');
    case 'Humidity':
      return numericHelper('Humidity');
    case 'Illuminance':
      return numericHelper('Illuminance');
    case 'SoilMoisture':
      return numericHelper('SoilMoisture');
    case 'Co2':
      return numericHelper('Co2');
    case 'Occupancy':
      return 'Extend::Occupancy';
    case 'Lock':
      return 'Extend::Lock';
    case 'IasZoneAlarm':
      return `Extend::IasZoneAlarm { alarms: ${vec((args.alarms ?? []).map((a) => `${owned(a)}`))} }`;
    case 'WindowCovering': {
      const controls = args.controls ?? [];
      return `Extend::WindowCovering { lift: ${controls.includes('lift')}, tilt: ${controls.includes('tilt')}, inverted: ${!!args.coverInverted} }`;
    }
    case 'ForcePowerSource': {
      const source = /batter/i.test(args.powerSource ?? '')
        ? 'PowerSourceHint::Battery'
        : /dc/i.test(args.powerSource ?? '')
          ? 'PowerSourceHint::Dc'
          : 'PowerSourceHint::Mains';
      return `Extend::ForcePowerSource { source: ${source} }`;
    }
    case 'TuyaBase':
      return `Extend::TuyaBase { datapoints: ${args.dp !== undefined}, query_on_announce: ${!!args.queryOnDeviceAnnounce}, query_interval_secs: ${opt(args.queryIntervalSeconds)} }`;
    case 'CommandsOnOff':
    case 'CommandsLevelCtrl': {
      const endpoints = endpointIds(args.endpointNames, endpointMap);
      if (endpoints === null) {
        return unsupported(helper, `endpoint names ${JSON.stringify(args.endpointNames)} are not in the endpoint map`);
      }
      const commands = vec((args.commands ?? []).map((c) => `${owned(c)}`));
      return `Extend::${helper} { commands: ${commands}, endpoints: ${vec(endpoints.map((e) => `EndpointId(${e})`))} }`;
    }
    case 'Numeric': {
      const ids = resolved();
      if (ids.why) return unsupported(helper, ids.why);
      const spec = numericSpec(args);
      if (spec.why) return unsupported(helper, spec.why);
      return `Extend::Numeric { name: ${owned(args.name)}, cluster: ClusterId(${ids.cluster}), attribute: AttrId(${ids.attribute}), spec: ${spec.spec}, access: ${access(args.access)} }`;
    }
    case 'Binary': {
      const ids = resolved();
      if (ids.why) return unsupported(helper, ids.why);
      // valueOn/valueOff arrive as [label, value]; the number is what goes on
      // the wire and the label is upstream's UI text.
      const value = (v) => (Array.isArray(v) ? v[1] : v);
      const on = value(args.valueOn);
      const off = value(args.valueOff);
      if (typeof on !== 'number' || typeof off !== 'number') {
        return unsupported(helper, `on/off values ${JSON.stringify([args.valueOn, args.valueOff])} are not numeric`);
      }
      return `Extend::Binary { name: ${owned(args.name)}, cluster: ClusterId(${ids.cluster}), attribute: AttrId(${ids.attribute}), value_on: ${on}, value_off: ${off}, access: ${access(args.access)} }`;
    }
    case 'EnumLookup': {
      const ids = resolved();
      if (ids.why) return unsupported(helper, ids.why);
      const values = Object.entries(args.lookup ?? {})
        .filter(([, v]) => typeof v === 'number')
        .map(([label, v]) => `(${v}, ${owned(label)})`);
      return `Extend::EnumLookup { name: ${owned(args.name)}, cluster: ClusterId(${ids.cluster}), attribute: AttrId(${ids.attribute}), values: ${vec(values)}, access: ${access(args.access)} }`;
    }
    case 'AddCustomCluster': {
      const id = clusterId(args.id);
      if (id === null) return unsupported(helper, `custom cluster ${args.name} has no numeric id`);
      // Attributes and commands are carried so a frame from this cluster can be
      // decoded at all; without them its payload has no known shape. Upstream
      // and `CustomAttribute`/`CustomCommand` are both tuples, in the same
      // order, so these map across directly.
      const attrs = (args.attributes ?? [])
        .filter(([id, , ty]) => typeof id === 'number' && typeof ty === 'number')
        .map(([id, name, ty]) => `(${id}, ${owned(name)}, ${ty})`);
      const cmds = (args.commands ?? [])
        .filter(([id]) => typeof id === 'number')
        .map(([id, name, params]) => {
          const rendered = (params ?? [])
            .filter(([, ty]) => typeof ty === 'number')
            .map(([pname, ty]) => `(${owned(pname)}, ${ty})`);
          return `(${id}, ${owned(name)}, ${vec(rendered)})`;
        });
      return `Extend::AddCustomCluster(CustomCluster { name: ${owned(args.name)}, id: ClusterId(${id}), manufacturer: ${opt(args.manufacturer)}, attributes: ${vec(attrs)}, commands: ${vec(cmds)}, responses: Vec::new() })`;
    }
    case 'Unsupported':
      return unsupported(args.helper ?? 'unknown', args.note ?? 'the transcoder could not express this');
    default:
      return unsupported(helper, 'no primitive in this build emits this helper');
  }
}

// --------------------------------------------------------------- match rules

function fingerprint(fp) {
  const fields = [];
  const strField = (key, name) => {
    if (fp[key] !== undefined && fp[key] !== null) fields.push(`${name}: Some(${owned(fp[key])})`);
  };
  const numField = (key, name) => {
    if (typeof fp[key] === 'number') fields.push(`${name}: Some(${fp[key]})`);
  };
  strField('modelID', 'model_id');
  strField('manufacturerName', 'manufacturer_name');
  numField('manufacturerID', 'manufacturer_id');
  numField('applicationVersion', 'application_version');
  numField('stackVersion', 'stack_version');
  numField('zclVersion', 'zcl_version');
  numField('hardwareVersion', 'hardware_version');
  strField('dateCode', 'date_code');
  strField('softwareBuildID', 'software_build_id');
  if (!fields.length) return null;
  return `Fingerprint { ${fields.join(', ')}, ..Default::default() }`;
}

function tuyaDatapoint(dp) {
  let kind;
  switch (dp.kind) {
    case 'Bool':
      kind = `TuyaKind::Bool { inverted: ${!!dp.inverted} }`;
      break;
    case 'Value': {
      const spec = numericSpec(dp);
      // `TuyaKind` has no unsupported variant, so a conversion that cannot be
      // represented becomes `Raw`: undecoded bytes claim nothing, where a
      // dropped divisor would claim a wrong number.
      kind = spec.why ? 'TuyaKind::Raw' : `TuyaKind::Value(${spec.spec})`;
      break;
    }
    case 'Enum':
      kind = `TuyaKind::Enum(${vec((dp.values ?? []).map(([v, label]) => `(${v}, ${owned(label)})`))})`;
      break;
    case 'Bitmap':
      kind = `TuyaKind::Bitmap(${vec((dp.values ?? []).map(([v, label]) => `(${v}, ${owned(label)})`))})`;
      break;
    case 'String':
      kind = 'TuyaKind::String';
      break;
    case 'Behavior':
      kind = `TuyaKind::Behavior { name: ${owned(dp.behavior)} }`;
      break;
    default:
      kind = 'TuyaKind::Raw';
  }
  return `TuyaDatapoint { dp: ${dp.dp}, name: ${owned(dp.name)}, kind: ${kind}, endpoint: None, access: Access::Report }`;
}

function binding(b) {
  const reporting = (b.reporting ?? []).map(
    (r) => (
      (stats.reportings += 1),
      `Reporting { attribute: AttrId(${r.attribute}), min_interval: ${r.minInterval ?? 10}, max_interval: ${r.maxInterval ?? 3600}, min_change: ${r.minChange ?? 0} }`
    ),
  );
  return `Binding { endpoint: EndpointId(${b.endpoint}), cluster: ClusterId(${b.cluster}), reporting: ${vec(reporting)} }`;
}

// ---------------------------------------------------------------- definition

function definition(ir) {
  stats.definitions += 1;

  // Built first: several helpers name endpoints and can only be resolved
  // against it.
  const endpointMap = new Map((ir.endpoints ?? []).map((e) => [String(e.name), e.id]));

  const extends_ = (ir.extend ?? []).map((e) => {
    stats.extends += 1;
    return extend(e, endpointMap);
  });

  const rules = rulesByModel.get(ir.model);
  const models = (rules?.z ?? ir.match?.models ?? []).map((m) => `${owned(m)}`);
  const fingerprints = (rules?.f ?? ir.match?.fingerprints ?? [])
    .map(fingerprint)
    .filter(Boolean);
  if (!models.length && !fingerprints.length) stats.unreachable += 1;

  const labels = (rules?.w ?? []).map((w) => {
    const fps = (w.fingerprints ?? []).map(fingerprint).filter(Boolean);
    return `WhiteLabel { model: ${owned(w.model)}, vendor: ${w.vendor ? `Some(${owned(w.vendor)})` : 'None'}, description: ${w.description ? `Some(${owned(w.description)})` : 'None'}, fingerprints: ${vec(fps)} }`;
  });

  const fields = [
    `model: ${owned(ir.model)}`,
    `vendor: ${owned(ir.vendor ?? '')}`,
    `description: ${owned(ir.description ?? '')}`,
    `match_rules: MatchRules { models: ${vec(models)}, fingerprints: ${vec(fingerprints)} }`,
  ];
  if (extends_.length) fields.push(`extend: ${vec(extends_)}`);
  if (ir.datapoints?.length) {
    fields.push(`tuya_datapoints: ${vec(ir.datapoints.map(tuyaDatapoint))}`);
  }
  if (ir.bindings?.length) fields.push(`bindings: ${vec(ir.bindings.map(binding))}`);
  if (endpointMap.size) {
    const names = [...endpointMap].map(([name, id]) => `(${owned(name)}, EndpointId(${id}))`);
    fields.push(`endpoint_names: ${vec(names)}`);
  }
  if (labels.length) fields.push(`white_labels: ${vec(labels)}`);

  return `        Definition {\n            ${fields.join(',\n            ')},\n            ..Default::default()\n        }`;
}

// --------------------------------------------------------------------- emit

const rendered = irs.map(definition);

// Chunked because one function with 4,473 struct literals in it makes rustc
// slow in a way that is felt on every incremental build.
const CHUNK = 250;
const chunks = [];
for (let i = 0; i < rendered.length; i += CHUNK) {
  chunks.push(rendered.slice(i, i + CHUNK));
}

const body = chunks
  .map(
    (chunk, i) =>
      `/// Definitions ${i * CHUNK}..${i * CHUNK + chunk.length}.\nfn chunk_${i}(out: &mut Vec<Definition>) {\n${chunk.map((d) => `    out.push(\n${d}\n    );`).join('\n')}\n}`,
  )
  .join('\n\n');

const calls = chunks.map((_, i) => `    chunk_${i}(&mut out);`).join('\n');

process.stdout.write(`//! Device definitions, generated from zigbee-herdsman-converters.
//!
//! **Do not edit.** Regenerate with \`scripts/refresh-device-coverage.sh\`.
//!
//! ${stats.definitions} definitions, ${stats.extends} capability references.
//! Transcoded from zigbee-herdsman-converters (MIT, (c) Koen Kanters and
//! contributors); cluster and attribute ids resolved against zigbee-herdsman's
//! own table so they agree with [\`rszigbee_spec::zcl\`] by construction.
//!
//! ${stats.unsupported} references are [\`Extend::Unsupported\`]. That is
//! recorded rather than dropped: a definition that quietly loses a capability
//! looks complete and behaves as though the device lacks it, and coverage stops
//! being measurable across upstream releases. A definition still identifies its
//! device and carries everything that *did* resolve.

// Generated code cannot be hand-formatted, and both of these are properties of
// the generator's output rather than of anything a reader could improve: the
// literals are upstream's declared limits, and the chunk functions are long
// because splitting 4,473 definitions into 250-definition functions is what
// keeps rustc from being slow on the whole file.
#![allow(clippy::unreadable_literal, clippy::too_many_lines)]

use rszigbee_spec::ids::{AttrId, ClusterId, EndpointId};

use crate::definition::{
    Access, Binding, CustomCluster, Definition, Extend, NumericSpec, PowerSourceHint,${stats.reportings ? ' Reporting,' : ''}
    TuyaDatapoint, TuyaKind, WhiteLabel,
};
use crate::matcher::{Fingerprint, MatchRules};

/// How many definitions this build ships.
pub const COUNT: usize = ${stats.definitions};

/// Every definition this build ships, in upstream's order.
///
/// Order matters: [\`DefinitionIndex\`](crate::DefinitionIndex) resolves
/// first-wins, so reordering would change which definition a device gets.
#[must_use]
pub fn definitions() -> Vec<Definition> {
    let mut out = Vec::with_capacity(COUNT);
${calls}
    out
}

${body}
`);

const top = (map, n) =>
  [...map.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, n)
    .map(([k, v]) => `${k} (${v})`)
    .join(', ');

process.stderr.write(
  `definitions: ${stats.definitions}, capability references: ${stats.extends}, ` +
    `unsupported: ${stats.unsupported}, fractional ranges dropped: ${stats.fractionalRanges}\n` +
    `unreachable (no model and no fingerprint): ${stats.unreachable}\n` +
    `unresolved clusters: ${stats.clusterMisses.size} distinct -- ${top(stats.clusterMisses, 6)}\n` +
    `unresolved attributes: ${stats.attributeMisses.size} distinct -- ${top(stats.attributeMisses, 4)}\n`,
);
