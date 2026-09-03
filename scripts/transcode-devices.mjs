// Transcodes zigbee-herdsman-converters definitions into rszigbee's
// declarative IR, and reports honestly on what it could not carry.
//
// Two outputs:
//   definitions.json  the IR, for rszigbee-devices to load and validate
//   coverage.json     one record per definition: classification, reasons,
//                     missing primitives, source location, what was generated
//
// The point of the second file is that a coverage number nobody can act on is
// a vanity metric. Every blocker is attributed to a *named primitive*, so the
// aggregate answers "which one thing should I implement next, and how many
// devices does it unlock" instead of "we support about three quarters".
//
// Reads the TypeScript source rather than the published JavaScript, because
// only the source still says which helper produced a behaviour. The compiled
// output has already flattened `extend` into anonymous converter arrays.
//
// zigbee-herdsman-converters is MIT, (c) 2018 Koen Kanters. This reads its data
// and emits our own representation of it; no code is copied or translated.

import fs from 'node:fs';
import path from 'node:path';
import ts from 'typescript';
import {Zcl} from 'zigbee-herdsman';

/**
 * Enum members upstream references by name, resolved to their values.
 *
 * `type: Zcl.DataType.SINGLE_PREC` is not a literal, so without this a custom
 * cluster's attributes cannot be typed and the whole definition is refused.
 * Taken from herdsman's own runtime enums rather than a copied table, so the
 * numbers cannot drift.
 */
const ENUMS = new Map();
for (const [group, table] of [
  ['DataType', Zcl.DataType],
  ['ManufacturerCode', Zcl.ManufacturerCode],
  ['Status', Zcl.Status],
]) {
  for (const [member, value] of Object.entries(table)) {
    if (typeof value === 'number') {
      ENUMS.set(`Zcl.${group}.${member}`, value);
    }
  }
}

// ---------------------------------------------------------------------------
// The primitives rszigbee actually implements today.
//
// Declared here, not inferred, so the coverage number cannot drift upward by
// accident. Adding a name here is a claim that `rszigbee-devices` can express
// it, and the Rust validator checks that claim.
// ---------------------------------------------------------------------------

/** `modernExtend` helpers that map onto an `Extend` variant. */
const KNOWN_EXTENDS = new Map([
  ['m.light', 'Light'],
  ['m.identify', 'Identify'],
  ['m.onOff', 'OnOff'],
  ['m.battery', 'Battery'],
  ['m.deviceEndpoints', 'DeviceEndpoints'],
  ['m.electricityMeter', 'ElectricityMeter'],
  ['m.temperature', 'Temperature'],
  ['m.humidity', 'Humidity'],
  ['m.illuminance', 'Illuminance'],
  ['m.soilMoisture', 'SoilMoisture'],
  ['m.co2', 'Co2'],
  ['m.occupancy', 'Occupancy'],
  ['m.iasZoneAlarm', 'IasZoneAlarm'],
  ['m.numeric', 'Numeric'],
  ['m.binary', 'Binary'],
  ['m.enumLookup', 'EnumLookup'],
  ['m.windowCovering', 'WindowCovering'],
  ['m.commandsOnOff', 'CommandsOnOff'],
  ['m.forcePowerSource', 'ForcePowerSource'],
  ['tuya.modernExtend.tuyaBase', 'TuyaBase'],
  ['m.deviceAddCustomCluster', 'AddCustomCluster'],
]);

/**
 * Vendor wrappers over a door lock.
 *
 * `lockExtend` (yale.ts:158) is lock state plus battery plus PIN and user
 * management. The lock itself is expressible; the PIN and user parts are not,
 * so this is an approximation for the same reason the light wrappers are: the
 * door locks and unlocks, and managing codes does not work.
 */
const VENDOR_LOCK_WRAPPERS = new Set(['lockExtend']);

/**
 * Vendor wrappers over `m.onOff`.
 *
 * Verified the same way as the lights: `philips.ts:764` and `tuya.ts:4920` are
 * `modernExtend.onOff(args)` with vendor extras layered on -- power-outage
 * memory, indicator mode, child lock, backlight, switch type. The switch
 * switches; the extras do not, so this is an approximation.
 */
const VENDOR_ONOFF_WRAPPERS = new Set([
  'philips.m.onOff',
  'ledvanceOnOff',
  'tuyaOnOff',
  'tuya.modernExtend.tuyaOnOff',
]);

/**
 * `fromZigbee` converters that map onto an `Extend`.
 *
 * These are shared library converters, not per-device code, so recognising
 * them is exactly the "implement one primitive, unlock N devices" case. The
 * IAS ones are the same cluster with different alarm names.
 */
const KNOWN_FZ = new Map([
  ['fz.ias_occupancy_alarm_1', {helper: 'IasZoneAlarm', args: {alarms: ['occupancy', 'tamper', 'battery_low']}}],
  ['fz.ias_contact_alarm_1', {helper: 'IasZoneAlarm', args: {alarms: ['contact', 'tamper', 'battery_low']}}],
  ['fz.ias_water_leak_alarm_1', {helper: 'IasZoneAlarm', args: {alarms: ['water_leak', 'tamper', 'battery_low']}}],
  ['fz.ias_smoke_alarm_1', {helper: 'IasZoneAlarm', args: {alarms: ['smoke', 'tamper', 'battery_low']}}],
  ['fz.ias_gas_alarm_1', {helper: 'IasZoneAlarm', args: {alarms: ['gas', 'tamper', 'battery_low']}}],
  ['fz.temperature', {helper: 'Temperature', args: {}}],
  ['fz.humidity', {helper: 'Humidity', args: {}}],
  ['fz.illuminance', {helper: 'Illuminance', args: {}}],
  ['fz.occupancy', {helper: 'Occupancy', args: {}}],
  ['fz.battery', {helper: 'Battery', args: {}}],
  ['fz.on_off', {helper: 'OnOff', args: {}}],
]);

/**
 * Converters the runtime already satisfies without a definition saying so.
 *
 * `linkquality_from_basic` reads the link quality out of any incoming frame,
 * and rszigbee carries it on every `ZclRx` already. Recognising it as
 * unnecessary is honest; reporting it as missing would rank a primitive that
 * needs no work.
 */
const SATISFIED_FZ = new Set([
  'fz.linkquality_from_basic',
  'fz.ignore_basic_report',
  'fz.ignore_genOta',
  'fz.ignore_time_read',
]);

/**
 * Vendor wrappers over `m.light`.
 *
 * Every one of these is literally `m.light(args)` with vendor defaults applied
 * and, in some cases, a vendor-native write path bolted on. Verified by reading
 * each: `philips.ts:312`, `gledopto.ts:324`, `ledvance.ts:99`,
 * `sengled.ts:19`, `muller_licht.ts:14`.
 *
 * Mapping them to `Light` is an **approximation**, not a full transcription:
 * the light itself works, the vendor's extra effects do not. It is recorded as
 * such rather than counted as complete, because a coverage number that quietly
 * includes approximations is the thing this whole report exists to prevent.
 *
 * Philips is worth one note: its native write path is an optimisation gated on
 * `hue_native_control === true`, which defaults off, so the standard converters
 * are what upstream itself uses by default.
 */
const VENDOR_LIGHT_WRAPPERS = new Set([
  'philips.m.light',
  'gledoptoLight',
  'ledvanceLight',
  'sengledLight',
  'mullerLichtLight',
  'sylvaniaLight',
  'osramLight',
  'tuyaLight',
  'tuya.modernExtend.tuyaLight',
]);

/** Tuya value converters whose semantics the IR can express. */
const KNOWN_TUYA_CONVERTERS = new Map([
  ['tuya.valueConverter.raw', {kind: 'Value', divisor: 1}],
  ['tuya.valueConverter.divideBy10', {kind: 'Value', divisor: 10}],
  ['tuya.valueConverter.divideBy100', {kind: 'Value', divisor: 100}],
  ['tuya.valueConverter.divideBy1000', {kind: 'Value', divisor: 1000}],
  ['tuya.valueConverter.onOff', {kind: 'Bool', inverted: false}],
  ['tuya.valueConverter.trueFalse0', {kind: 'Bool', inverted: true}],
  ['tuya.valueConverter.trueFalse1', {kind: 'Bool', inverted: false}],
  ['tuya.valueConverter.batteryState', {kind: 'Enum'}],
  // Plain integers with their own capability meaning.
  ['tuya.valueConverter.countdown', {kind: 'Value', divisor: 1}],
  ['tuya.valueConverter.coverPosition', {kind: 'Value', divisor: 1}],
  ['tuya.valueConverter.lockUnlock', {kind: 'Bool', inverted: false}],
  ['tuya.valueConverterBasic.divideBy', {kind: 'Value', divisor: 1}],
]);

/**
 * Tuya converters this build deliberately does not claim.
 *
 * Each needs behaviour the datapoint table cannot express, and claiming one
 * would report a device as usable while its values came out wrong:
 *
 * * `scale0_254to0_1000` is a range remap, not a divisor.
 * * `coverPositionInverted` needs an inversion the numeric spec has no field
 *   for, and getting it wrong closes a blind asked to open.
 * * `static` substitutes a constant rather than converting anything.
 * * the `threshold_*` and `phaseVariant*` converters unpack several values
 *   from one datapoint.
 *
 * Listed rather than silently missing so the ranking shows them as work with a
 * known shape.
 */
const UNEXPRESSIBLE_TUYA = new Set([
  'tuya.valueConverter.scale0_254to0_1000',
  'tuya.valueConverter.coverPositionInverted',
  'tuya.valueConverter.static',
  'tuya.valueConverter.threshold_2',
  'tuya.valueConverter.threshold_3',
  'tuya.valueConverter.phaseVariant2WithPhase',
  'tuya.valueConverter.thermostatScheduleDayMultiDPWithDayNumber',
  'tuya.valueConverter.utf16BEHexString',
]);

/**
 * Extracts a `valueConverterBasic.lookup({...})` mapping.
 *
 * Values are either `tuya.enum(N)` or a boolean. Booleans map to 1 and 0 so
 * one table shape serves both wire types — a device can report the same
 * datapoint as either, and firmware revisions change which.
 *
 * Returns undefined if any entry cannot be evaluated: a partial lookup reports
 * some values by name and the rest by number, which reads like two different
 * devices.
 */
function lookupTable(node) {
  if (!ts.isCallExpression(node)) return undefined;
  const arg = node.arguments[0];
  if (!arg || !ts.isObjectLiteralExpression(arg)) return undefined;

  const values = [];
  for (const entry of arg.properties) {
    if (!ts.isPropertyAssignment(entry)) return undefined;
    const name = entry.name.getText().replace(/["']/g, '');
    const value = entry.initializer;
    if (value.kind === ts.SyntaxKind.TrueKeyword) {
      values.push([1, name]);
      continue;
    }
    if (value.kind === ts.SyntaxKind.FalseKeyword) {
      values.push([0, name]);
      continue;
    }
    // `tuya.enum(0)`, `tuya.bitmap(...)` and friends.
    if (ts.isCallExpression(value)) {
      const inner = literal(value.arguments[0]);
      if (typeof inner === 'number') {
        values.push([inner, name]);
        continue;
      }
    }
    const plain = literal(value);
    if (typeof plain === 'number') {
      values.push([plain, name]);
      continue;
    }
    return undefined;
  }
  return values.length > 0 ? {kind: 'Enum', values} : undefined;
}

/** `exposes` presets the IR can express. */
const KNOWN_EXPOSES = new Set([
  'e.temperature', 'e.humidity', 'e.soil_moisture', 'e.co2', 'e.illuminance',
  'e.occupancy', 'e.battery', 'e.battery_low', 'e.battery_voltage',
  'e.switch', 'e.contact', 'e.water_leak', 'e.tamper', 'e.presence',
  'e.power', 'e.energy', 'e.current', 'e.voltage', 'e.device_temperature',
  'e.numeric', 'e.binary', 'e.enum', 'e.text',
]);

/**
 * `reporting.*` helpers whose bind-and-report effect the IR can express.
 *
 * Each reduces to one row in a bindings table. Anything else in a `configure`
 * body is imperative and cannot.
 */
const KNOWN_REPORTING = new Set([
  'reporting.bind', 'reporting.onOff', 'reporting.batteryPercentageRemaining',
  'reporting.batteryVoltage', 'reporting.temperature', 'reporting.humidity',
  'reporting.illuminance', 'reporting.occupancy', 'reporting.activePower',
  'reporting.rmsVoltage', 'reporting.rmsCurrent', 'reporting.currentSummDelivered',
  'reporting.brightness', 'reporting.deviceTemperature',
]);


/**
 * Cluster names to ids.
 *
 * Upstream's `reporting.bind` names clusters rather than numbering them, so a
 * binding cannot be transcoded without this. Ids are from the ZCL
 * specification; a Rust test checks every one against the cluster registry, so
 * a wrong number here fails the build rather than producing a binding to the
 * wrong cluster.
 *
 * Vendor-specific names are deliberately absent: they resolve to different ids
 * per manufacturer, and guessing one would bind to whatever happens to live
 * there. They are reported as missing primitives instead.
 */
const CLUSTER_IDS = new Map([
  ['genBasic', 0x0000], ['genPowerCfg', 0x0001], ['genIdentify', 0x0003],
  ['genGroups', 0x0004], ['genScenes', 0x0005], ['genOnOff', 0x0006],
  ['genLevelCtrl', 0x0008], ['genBinaryInput', 0x000f], ['genOta', 0x0019],
  ['genPollCtrl', 0x0020], ['genTime', 0x000a],
  ['closuresDoorLock', 0x0101], ['closuresWindowCovering', 0x0102],
  ['hvacThermostat', 0x0201], ['hvacFanCtrl', 0x0202],
  ['hvacUserInterfaceCfg', 0x0204],
  ['lightingColorCtrl', 0x0300], ['lightingBallastCfg', 0x0301],
  ['msIlluminanceMeasurement', 0x0400], ['msTemperatureMeasurement', 0x0402],
  ['msPressureMeasurement', 0x0403], ['msRelativeHumidity', 0x0405],
  ['msOccupancySensing', 0x0406], ['msSoilMoisture', 0x0408], ['msCO2', 0x040d],
  ['ssIasZone', 0x0500], ['ssIasWd', 0x0502],
  ['seMetering', 0x0702], ['haElectricalMeasurement', 0x0b04],
  ['haDiagnostic', 0x0b05],
]);

/**
 * Calls inside `configure` that need no binding of their own.
 *
 * `reporting.X(endpoint)` configures reporting for an attribute the capability
 * sources already cover, and `endpoint.read` is an interview-time read. Both
 * are recognised so they do not make an otherwise transcodable body look
 * imperative.
 */
const CONFIGURE_IGNORABLE = [
  /^reporting\./, /^endpoint\.read$/, /^endpoint\.write$/,
  /^device\.(save|getEndpoint|powerSource)$/,
  /^tuya\.configureMagicPacket$/,
  /^endpoint\.saveClusterAttributeKeyValue$/,
  /^m\./, /^utils\.assertEndpoint$/,
];

// ---------------------------------------------------------------------------
// Small AST helpers
// ---------------------------------------------------------------------------

/** Reads a property off an object literal, or `undefined`. */
function prop(obj, name) {
  if (!obj || !ts.isObjectLiteralExpression(obj)) return undefined;
  for (const p of obj.properties) {
    if (ts.isPropertyAssignment(p) && p.name.getText().replace(/["']/g, '') === name) {
      return p.initializer;
    }
  }
  return undefined;
}

/** Every top-level property name on an object literal. */
function propNames(obj) {
  if (!obj || !ts.isObjectLiteralExpression(obj)) return [];
  return obj.properties
    .filter((p) => ts.isPropertyAssignment(p) || ts.isShorthandPropertyAssignment(p))
    .map((p) => p.name.getText().replace(/["']/g, ''));
}

/**
 * Evaluates a node to a plain JSON value, or returns the NOT_LITERAL sentinel.
 *
 * Deliberately refuses anything it cannot evaluate exactly. A transcoder that
 * guessed at a value would emit a definition that looks right and behaves
 * wrong, which is worse than reporting the gap.
 */
const NOT_LITERAL = Symbol('not-literal');
function literal(node) {
  if (!node) return NOT_LITERAL;
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) return node.text;
  if (ts.isNumericLiteral(node)) return Number(node.text);
  if (node.kind === ts.SyntaxKind.TrueKeyword) return true;
  if (node.kind === ts.SyntaxKind.FalseKeyword) return false;
  if (node.kind === ts.SyntaxKind.NullKeyword) return null;
  // `undefined` is an identifier in TypeScript, not a keyword. It appears as a
  // deliberate value: `{colorTemp: {range: undefined}}` means "the device
  // reports its own range, do not hard-code one", which is expressible. Left
  // as non-literal it was the single largest blocker in the report.
  if (ts.isIdentifier(node) && node.text === 'undefined') return null;
  // `Zcl.DataType.SINGLE_PREC` and friends, resolved from herdsman's enums.
  if (ts.isPropertyAccessExpression(node)) {
    const resolved = ENUMS.get(node.getText().replace(/\s+/g, ''));
    if (resolved !== undefined) return resolved;
  }
  if (ts.isPrefixUnaryExpression(node) && node.operator === ts.SyntaxKind.MinusToken) {
    const inner = literal(node.operand);
    return typeof inner === 'number' ? -inner : NOT_LITERAL;
  }
  // Concatenation, which upstream uses to keep a long NUL-padded model string
  // readable: `" Contactor\u0000..." + "\u0000..."`. Two Legrand devices are
  // written this way, and without this they had no model string at all -- found
  // by cross-validating against upstream's runtime, not by inspection.
  if (ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.PlusToken) {
    const left = literal(node.left);
    const right = literal(node.right);
    if (typeof left === 'string' && typeof right === 'string') return left + right;
    if (typeof left === 'number' && typeof right === 'number') return left + right;
    return NOT_LITERAL;
  }
  if (ts.isArrayLiteralExpression(node)) {
    const out = [];
    for (const el of node.elements) {
      const v = literal(el);
      if (v === NOT_LITERAL) return NOT_LITERAL;
      out.push(v);
    }
    return out;
  }
  if (ts.isObjectLiteralExpression(node)) {
    const out = {};
    for (const p of node.properties) {
      if (!ts.isPropertyAssignment(p)) return NOT_LITERAL;
      const v = literal(p.initializer);
      if (v === NOT_LITERAL) return NOT_LITERAL;
      out[p.name.getText().replace(/["']/g, '')] = v;
    }
    return out;
  }
  return NOT_LITERAL;
}

/** The dotted name of a call's callee, e.g. `m.temperature`. */
function calleeName(node) {
  if (!ts.isCallExpression(node)) return undefined;
  // Whitespace collapsed, because upstream formats long fluent chains across
  // lines: `e\n    .numeric(...)`. Left raw, the name carries a newline and an
  // indent and matches nothing -- which silently blocked 305 definitions on a
  // primitive that was already implemented.
  return node.expression.getText().replace(/\s+/g, '');
}

/** Walks a fluent chain back to its root call, collecting `.withX(...)` steps. */
function unchain(node) {
  const steps = [];
  let cur = node;
  while (ts.isCallExpression(cur) && ts.isPropertyAccessExpression(cur.expression)) {
    const name = cur.expression.name.getText();
    if (!name.startsWith('with')) break;
    steps.unshift({name, args: cur.arguments.map(literal)});
    cur = cur.expression.expression;
  }
  return {root: cur, steps};
}

// ---------------------------------------------------------------------------
// Per-section transcoding. Each returns {ok, value, missing[]}.
// ---------------------------------------------------------------------------

function transcodeMatchRules(def) {
  const missing = [];
  const models = [];
  const zm = prop(def, 'zigbeeModel');
  if (zm) {
    const v = literal(zm);
    if (v === NOT_LITERAL) missing.push({primitive: 'zigbeeModel:non-literal', kind: 'data'});
    else models.push(...v);
  }

  const fingerprints = [];
  const fp = prop(def, 'fingerprint');
  if (fp && ts.isArrayLiteralExpression(fp)) {
    for (const entry of fp.elements) {
      const v = literal(entry);
      if (v === NOT_LITERAL) {
        missing.push({primitive: 'fingerprint:non-literal', kind: 'data'});
        continue;
      }
      // `ieeeAddr` is a regex in the source, so a literal() of it fails; it is
      // handled separately below by reading the raw text.
      fingerprints.push(v);
    }
    // Recover regex fingerprints, checking the prefix-equivalence our matcher
    // relies on rather than assuming it.
    for (const entry of fp.elements) {
      const ieee = prop(entry, 'ieeeAddr');
      if (!ieee) continue;
      const text = ieee.getText();
      const m = /^\/\^([0-9a-fx]+)(\.*)\$\/$/i.exec(text);
      if (m && m[1].length + m[2].length === 18) {
        fingerprints.push({ieeePrefix: m[1]});
      } else {
        missing.push({primitive: 'fingerprint:ieeeAddr-regex', kind: 'rust', detail: text});
      }
    }
  }
  return {value: {models, fingerprints}, missing};
}

function transcodeExtend(def) {
  const missing = [];
  const approximations = [];
  const out = [];
  const ext = prop(def, 'extend');
  if (!ext) return {value: out, missing};
  if (!ts.isArrayLiteralExpression(ext)) {
    missing.push({primitive: 'extend:non-array', kind: 'rust'});
    return {value: out, missing};
  }
  for (const el of ext.elements) {
    // `...vendorPreset()` splices in a helper array. The contents are not
    // visible here, so it is reported rather than guessed at.
    if (ts.isSpreadElement(el)) {
      const spread = calleeName(el.expression) ?? el.expression.getText();
      missing.push({primitive: `spread:${spread}`, kind: 'primitive'});
      continue;
    }
    const name = calleeName(el);
    if (!name) {
      missing.push({primitive: 'extend:non-call', kind: 'rust', detail: el.getText().slice(0, 60)});
      continue;
    }
    const known = KNOWN_EXTENDS.get(name);
    const args = ts.isCallExpression(el) ? el.arguments.map(literal) : [];
    const nonLiteral = args.some((a) => a === NOT_LITERAL);
    // A manufacturer-specific cluster, carried as data. Without it a frame
    // from that cluster cannot be decoded at all: its attributes have no known
    // types, so the device reports nothing usable.
    if (name === 'm.deviceAddCustomCluster') {
      const custom = customCluster(el);
      if (custom) {
        out.push({helper: 'AddCustomCluster', args: custom});
      } else {
        missing.push({primitive: 'm.deviceAddCustomCluster:shape', kind: 'rust'});
      }
      continue;
    }
    if (!known && VENDOR_ONOFF_WRAPPERS.has(name)) {
      out.push({helper: 'OnOff', args: {}});
      approximations.push({
        primitive: name,
        why: 'vendor on/off wrapper: switching works, vendor extras are not expressed',
      });
      continue;
    }
    if (!known && VENDOR_LOCK_WRAPPERS.has(name)) {
      out.push({helper: 'Lock', args: {}});
      out.push({helper: 'Battery', args: {}});
      approximations.push({
        primitive: name,
        why: 'vendor lock wrapper: locking works, PIN and user management are not expressed',
      });
      continue;
    }
    if (!known && VENDOR_LIGHT_WRAPPERS.has(name)) {
      out.push({helper: 'Light', args: nonLiteral ? {} : (args[0] ?? {})});
      approximations.push({primitive: name, why: 'vendor light wrapper: the light works, vendor-specific effects are not expressed'});
      continue;
    }
    if (!known) {
      missing.push({primitive: name, kind: 'primitive'});
      continue;
    }
    if (nonLiteral) {
      missing.push({primitive: `${name}:non-literal-args`, kind: 'rust'});
      continue;
    }
    out.push({helper: known, args: args[0] ?? {}});
  }
  return {value: out, missing, approximations};
}

/**
 * Extracts a custom cluster definition from a `deviceAddCustomCluster` call.
 *
 * Returns undefined when any part cannot be evaluated exactly. A partially
 * transcoded custom cluster is worse than none: an attribute with a guessed
 * type decodes to the wrong value, silently.
 */
function customCluster(call) {
  const args = call.arguments;
  if (args.length < 2) return undefined;
  const spec = args[1];
  if (!ts.isObjectLiteralExpression(spec)) return undefined;

  const id = literal(prop(spec, 'ID'));
  if (typeof id !== 'number') return undefined;
  const name = literal(args[0]) ?? literal(prop(spec, 'name'));
  if (typeof name !== 'string') return undefined;
  const manufacturer = literal(prop(spec, 'manufacturerCode'));

  /** `{attrName: {ID, type}}` to `[[id, name, tag]]`. */
  const attributes = [];
  const attrNode = prop(spec, 'attributes');
  if (attrNode && ts.isObjectLiteralExpression(attrNode)) {
    for (const entry of attrNode.properties) {
      if (!ts.isPropertyAssignment(entry)) return undefined;
      const key = entry.name.getText().replace(/["']/g, '');
      const attrId = literal(prop(entry.initializer, 'ID'));
      const type = literal(prop(entry.initializer, 'type'));
      if (typeof attrId !== 'number' || typeof type !== 'number') return undefined;
      attributes.push([attrId, key, type]);
    }
  }

  /** `{cmdName: {ID, parameters}}` to `[[id, name, [[param, tag]]]]`. */
  const readCommands = (key) => {
    const node = prop(spec, key);
    const out = [];
    if (!node || !ts.isObjectLiteralExpression(node)) return out;
    for (const entry of node.properties) {
      if (!ts.isPropertyAssignment(entry)) return null;
      const cmdName = entry.name.getText().replace(/["']/g, '');
      const cmdId = literal(prop(entry.initializer, 'ID'));
      if (typeof cmdId !== 'number') return null;
      const params = [];
      const paramNode = prop(entry.initializer, 'parameters');
      if (paramNode && ts.isArrayLiteralExpression(paramNode)) {
        for (const p of paramNode.elements) {
          const pName = literal(prop(p, 'name'));
          const pType = literal(prop(p, 'type'));
          // A composite parameter type has no representation, so the whole
          // cluster is refused rather than emitted with a wrong one.
          if (typeof pName !== 'string' || typeof pType !== 'number' || pType > 255) {
            return null;
          }
          params.push([pName, pType]);
        }
      }
      out.push([cmdId, cmdName, params]);
    }
    return out;
  };

  const commands = readCommands('commands');
  const responses = readCommands('commandsResponse');
  if (commands === null || responses === null) return undefined;

  return {
    name,
    id,
    manufacturer: typeof manufacturer === 'number' ? manufacturer : null,
    attributes,
    commands,
    responses,
  };
}

function transcodeExposes(def) {
  const missing = [];
  const out = [];
  const ex = prop(def, 'exposes');
  if (!ex) return {value: out, missing};
  if (!ts.isArrayLiteralExpression(ex)) {
    // A function `exposes` is device-dependent; only 1.6% of the catalogue.
    missing.push({primitive: 'exposes:function', kind: 'rust'});
    return {value: out, missing};
  }
  for (const el of ex.elements) {
    const {root, steps} = unchain(el);
    const name = calleeName(root);
    if (!name) {
      missing.push({primitive: 'exposes:non-call', kind: 'rust', detail: el.getText().slice(0, 50)});
      continue;
    }
    if (!KNOWN_EXPOSES.has(name)) {
      missing.push({primitive: name, kind: 'primitive'});
      continue;
    }
    const args = ts.isCallExpression(root) ? root.arguments.map(literal) : [];
    const entry = {preset: name, args: args.filter((a) => a !== NOT_LITERAL)};
    for (const step of steps) {
      if (step.args.some((a) => a === NOT_LITERAL)) continue;
      entry[step.name] = step.args.length === 1 ? step.args[0] : step.args;
    }
    out.push(entry);
  }
  return {value: out, missing};
}

function transcodeTuyaDatapoints(def) {
  const missing = [];
  const out = [];
  const meta = prop(def, 'meta');
  const dps = prop(meta, 'tuyaDatapoints');
  if (!dps) return {value: out, missing};
  if (!ts.isArrayLiteralExpression(dps)) {
    missing.push({primitive: 'tuyaDatapoints:non-array', kind: 'rust'});
    return {value: out, missing};
  }
  for (const el of dps.elements) {
    // Each entry is `[dp, name, converter]`, sometimes with a fourth element.
    if (!ts.isArrayLiteralExpression(el) || el.elements.length < 3) {
      missing.push({primitive: 'tuyaDatapoints:shape', kind: 'rust', detail: el.getText().slice(0, 50)});
      continue;
    }
    const dp = literal(el.elements[0]);
    const name = literal(el.elements[1]);
    const converter = el.elements[2].getText().replace(/\s+/g, '');
    if (dp === NOT_LITERAL || name === NOT_LITERAL) {
      missing.push({primitive: 'tuyaDatapoints:non-literal', kind: 'rust'});
      continue;
    }
    const known = KNOWN_TUYA_CONVERTERS.get(converter);
    if (known) {
      out.push({dp, name, ...known});
      continue;
    }
    // A lookup carries its own mapping, so it is expressible whenever the
    // mapping is literal.
    if (/valueConverterBasic\.lookup$/.test(converter.replace(/\(.*$/, ''))
        || converter.includes('valueConverterBasic.lookup(')) {
      const table = lookupTable(el.elements[2]);
      if (table) {
        out.push({dp, name, ...table});
        continue;
      }
    }
    missing.push({
      primitive: converter.replace(/\(.*$/, ''),
      kind: UNEXPRESSIBLE_TUYA.has(converter.replace(/\(.*$/, '')) ? 'rust' : 'primitive',
    });
  }
  return {value: out, missing};
}

function transcodeEndpoints(def) {
  const missing = [];
  const ep = prop(def, 'endpoint');
  if (!ep) return {value: [], missing};
  // `(device) => ({left: 1, right: 2})`
  if (!ts.isArrowFunction(ep)) {
    missing.push({primitive: 'endpoint:not-an-arrow', kind: 'rust'});
    return {value: [], missing};
  }
  let body = ep.body;
  if (ts.isParenthesizedExpression(body)) body = body.expression;
  // `(device) => { return {left: 1}; }` is the same table as
  // `(device) => ({left: 1})`, just written with a block.
  if (ts.isBlock(body)) {
    const statements = body.statements.filter((s) => !ts.isEmptyStatement(s));
    if (statements.length === 1 && ts.isReturnStatement(statements[0]) && statements[0].expression) {
      body = statements[0].expression;
      if (ts.isParenthesizedExpression(body)) body = body.expression;
    }
  }
  const v = literal(body);
  if (v === NOT_LITERAL || typeof v !== 'object' || v === null) {
    missing.push({primitive: 'endpoint:computed', kind: 'rust'});
    return {value: [], missing};
  }
  return {value: Object.entries(v).map(([name, id]) => ({name, id})), missing};
}

function transcodeConfigure(def) {
  const missing = [];
  const cfg = prop(def, 'configure');
  if (!cfg) return {value: [], missing};
  if (!ts.isArrowFunction(cfg) && !ts.isFunctionExpression(cfg)) {
    missing.push({primitive: 'configure:not-a-function', kind: 'rust'});
    return {value: [], missing};
  }

  const bindings = [];
  // Local names bound to an endpoint number, e.g.
  // `const endpoint = device.getEndpoint(1)`.
  const endpointVars = new Map();
  // Loop variables bound to a literal, while unrolling.
  const loopVars = new Map();
  let blocked = null;

  /** Resolves an expression to an endpoint number, or undefined. */
  const endpointOf = (node) => {
    if (!node) return undefined;
    if (ts.isNumericLiteral(node)) return Number(node.text);
    if (ts.isIdentifier(node)) {
      const name = node.text;
      if (loopVars.has(name)) return loopVars.get(name);
      if (endpointVars.has(name)) return endpointVars.get(name);
      return undefined;
    }
    if (ts.isCallExpression(node) && /^device\.getEndpoint$/.test(node.expression.getText())) {
      return endpointOf(node.arguments[0]);
    }
    return undefined;
  };

  /** Records the bindings one `reporting.bind(...)` call asks for. */
  const takeBind = (call) => {
    const endpoint = endpointOf(call.arguments[0]);
    if (endpoint === undefined) {
      blocked = 'configure:bind-endpoint-not-literal';
      return;
    }
    const names = literal(call.arguments[2]);
    if (names === NOT_LITERAL || !Array.isArray(names)) {
      blocked = 'configure:bind-clusters-not-literal';
      return;
    }
    for (const name of names) {
      const id = CLUSTER_IDS.get(name);
      if (id === undefined) {
        // A vendor cluster whose id differs per manufacturer. Reported by
        // name, so it ranks like any other missing primitive.
        missing.push({primitive: `cluster:${name}`, kind: 'primitive'});
        continue;
      }
      bindings.push({endpoint, cluster: id, reporting: []});
    }
  };

  const visit = (node) => {
    if (blocked) return;

    // `const endpoint = device.getEndpoint(1)`
    if (ts.isVariableStatement(node)) {
      for (const decl of node.declarationList.declarations) {
        const value = endpointOf(decl.initializer);
        if (value !== undefined && ts.isIdentifier(decl.name)) {
          endpointVars.set(decl.name.text, value);
        } else if (decl.initializer && !ts.isIdentifier(decl.name)) {
          blocked = 'configure:destructuring';
          return;
        }
      }
      return;
    }

    // `for (const ep of [1, 2, 3]) { ... }` -- a table written as a loop.
    // Unrolled rather than refused: every one of these in the catalogue binds
    // fixed clusters on a literal list of endpoints.
    if (ts.isForOfStatement(node)) {
      const items = literal(node.expression);
      if (items === NOT_LITERAL || !Array.isArray(items)) {
        blocked = 'configure:loop-not-literal';
        return;
      }
      const decl = node.initializer;
      if (!ts.isVariableDeclarationList(decl) || decl.declarations.length !== 1) {
        blocked = 'configure:loop-shape';
        return;
      }
      const name = decl.declarations[0].name;
      if (!ts.isIdentifier(name)) {
        blocked = 'configure:loop-shape';
        return;
      }
      for (const item of items) {
        if (typeof item !== 'number') {
          blocked = 'configure:loop-not-endpoints';
          return;
        }
        loopVars.set(name.text, item);
        ts.forEachChild(node.statement, visit);
        if (blocked) return;
      }
      loopVars.delete(name.text);
      return;
    }

    if (ts.isIfStatement(node) || ts.isWhileStatement(node) || ts.isForStatement(node)
        || ts.isSwitchStatement(node) || ts.isTryStatement(node)
        || ts.isConditionalExpression(node)) {
      blocked = 'configure:imperative';
      return;
    }

    if (ts.isCallExpression(node)) {
      const name = node.expression.getText().replace(/\s+/g, '');
      if (/^reporting\.bind$/.test(name)) {
        takeBind(node);
        return;
      }
      if (!CONFIGURE_IGNORABLE.some((re) => re.test(name))) {
        missing.push({primitive: `configure:${name}`, kind: 'primitive'});
        blocked = null;
      }
    }
    ts.forEachChild(node, visit);
  };

  visit(cfg.body);

  if (blocked) {
    missing.push({primitive: blocked, kind: 'rust'});
    return {value: [], missing};
  }
  // Deduplicated: the same endpoint and cluster bound twice is one binding.
  const seen = new Set();
  const unique = bindings.filter((b) => {
    const key = `${b.endpoint}:${b.cluster}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  return {value: unique, missing};
}

// ---------------------------------------------------------------------------
// One definition
// ---------------------------------------------------------------------------

const DATA_KEYS = new Set([
  'zigbeeModel', 'fingerprint', 'model', 'vendor', 'description', 'extend',
  'meta', 'whiteLabel', 'options', 'exposes', 'configure', 'endpoint', 'ota',
  'icon', 'generateCodeSnippet', 'externalConverterName', 'version',
]);

function transcode(def, file, line) {
  const model = literal(prop(def, 'model'));
  const vendor = literal(prop(def, 'vendor'));
  const description = literal(prop(def, 'description'));

  const missing = [];
  const approximations = [];
  const sections = {};
  const push = (name, result) => {
    sections[name] = result.missing.length === 0;
    missing.push(...result.missing);
    approximations.push(...(result.approximations ?? []));
    return result.value;
  };

  const match = push('match', transcodeMatchRules(def));
  const extend = push('extend', transcodeExtend(def));
  const exposes = push('exposes', transcodeExposes(def));
  const datapoints = push('datapoints', transcodeTuyaDatapoints(def));
  const endpoints = push('endpoints', transcodeEndpoints(def));
  const bindings = push('configure', transcodeConfigure(def));

  // Anything referencing a converter list or a file-local symbol is code.
  for (const key of ['fromZigbee', 'toZigbee']) {
    const node = prop(def, key);
    if (!node || !ts.isArrayLiteralExpression(node)) continue;
    let unresolved = 0;
    for (const el of node.elements) {
      const name = el.getText().replace(/\s+/g, '');
      const mapped = KNOWN_FZ.get(name);
      if (mapped) {
        extend.push(mapped);
        continue;
      }
      if (SATISFIED_FZ.has(name)) {
        continue;
      }
      if (/^(fz|tz)\./.test(name)) {
        missing.push({primitive: name, kind: 'primitive'});
      } else {
        missing.push({primitive: `${key}:local`, kind: 'rust', detail: name.slice(0, 40)});
      }
      unresolved += 1;
    }
    sections[key] = unresolved === 0;
  }

  const keys = propNames(def);
  for (const key of keys) {
    if (!DATA_KEYS.has(key) && key !== 'fromZigbee' && key !== 'toZigbee') {
      missing.push({primitive: `key:${key}`, kind: 'rust'});
    }
  }
  if (/\blegacy\./.test(def.getText())) {
    missing.push({primitive: 'legacy', kind: 'unsupported'});
  }

  // ---- classification
  const kinds = new Set(missing.map((m) => m.kind));
  let classification;
  if (missing.length === 0 && approximations.length === 0) classification = 'complete';
  // Usable, but not a faithful transcription. Kept as its own bucket so the
  // headline number cannot quietly absorb it.
  else if (missing.length === 0) classification = 'approximate';
  else if (kinds.has('unsupported')) classification = 'unsupported';
  else if (kinds.has('rust')) classification = 'needs-rust';
  else classification = 'needs-primitive';

  const generatedAnything =
    match.models.length > 0 || match.fingerprints.length > 0;

  return {
    ir: {
      model: model === NOT_LITERAL ? null : model,
      vendor: vendor === NOT_LITERAL ? null : vendor,
      description: description === NOT_LITERAL ? null : description,
      match,
      extend,
      exposes,
      datapoints,
      endpoints,
      bindings,
      complete: classification === 'complete',
      approximate: classification === 'approximate',
    },
    report: {
      model: model === NOT_LITERAL ? '(non-literal)' : model,
      source: `${file}:${line}`,
      classification,
      sections,
      // Deduplicated so one definition referencing the same converter twice
      // does not inflate the ranking.
      missing: [...new Map(missing.map((m) => [m.primitive, m])).values()],
      approximations: [...new Map(approximations.map((a) => [a.primitive, a])).values()],
      generated: generatedAnything,
    },
  };
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

function definitionsIn(sourceFile) {
  let found = null;
  const walk = (node) => {
    if (found) return;
    if (ts.isVariableDeclaration(node)
        && node.name.getText() === 'definitions'
        && node.initializer
        && ts.isArrayLiteralExpression(node.initializer)) {
      found = node.initializer;
      return;
    }
    ts.forEachChild(node, walk);
  };
  walk(sourceFile);
  return found ? found.elements.filter(ts.isObjectLiteralExpression) : [];
}

const srcDir = process.argv[2];
const harvested = process.argv[3];
if (!srcDir || !fs.existsSync(srcDir)) {
  console.error('usage: transcode-devices.mjs <zhc src/devices> [harvested match-rules.json]');
  process.exit(2);
}

const irs = [];
const reports = [];
for (const name of fs.readdirSync(srcDir).sort()) {
  if (!name.endsWith('.ts') || name === 'index.ts') continue;
  const file = path.join(srcDir, name);
  const sf = ts.createSourceFile(file, fs.readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true);
  for (const def of definitionsIn(sf)) {
    const {line} = sf.getLineAndCharacterOfPosition(def.getStart());
    const {ir, report} = transcode(def, name, line + 1);
    irs.push(ir);
    reports.push(report);
  }
}

fs.writeFileSync('definitions.json', JSON.stringify(irs));
fs.writeFileSync('coverage.json', JSON.stringify(reports));

// ---- the aggregate that makes the number actionable
const byClass = new Map();
for (const r of reports) byClass.set(r.classification, (byClass.get(r.classification) ?? 0) + 1);

const blockedBy = new Map();
for (const r of reports) {
  if (r.classification === 'complete' || r.classification === 'approximate') continue;
  for (const m of r.missing) {
    if (!blockedBy.has(m.primitive)) blockedBy.set(m.primitive, {kind: m.kind, devices: 0, soleBlocker: 0});
    const entry = blockedBy.get(m.primitive);
    entry.devices += 1;
    if (r.missing.length === 1) entry.soleBlocker += 1;
  }
}

const total = reports.length;
console.log(`definitions transcoded: ${total}\n`);
console.log('=== classification ===');
for (const [k, v] of [...byClass].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${String(v).padStart(5)}  ${(100 * v / total).toFixed(1).padStart(5)}%  ${k}`);
}

console.log('\n=== missing primitives, ranked by devices unblocked if implemented ===');
console.log('  (sole = this is the *only* thing blocking that definition)');
const ranked = [...blockedBy].sort((a, b) => b[1].soleBlocker - a[1].soleBlocker || b[1].devices - a[1].devices);
for (const [name, e] of ranked.slice(0, 40)) {
  console.log(`  ${String(e.soleBlocker).padStart(4)} sole  ${String(e.devices).padStart(5)} total  ${e.kind.padEnd(11)}  ${name}`);
}
console.log(`\ndistinct missing primitives: ${blockedBy.size}`);

// A usable definition is complete *or* a documented approximation. Both are
// reported, never merged: the first is a transcription, the second is a
// device that works with something missing.
const usable = reports.filter((r) => r.classification === 'complete'
                                  || r.classification === 'approximate').length;
console.log(`\nusable (complete + approximate): ${usable} / ${total} = ${(100 * usable / total).toFixed(1)}%`);

// ---------------------------------------------------------------------------
// Cross-validate the extracted match rules against upstream's own runtime.
//
// This is what makes resolution agreement follow by construction rather than
// by a second differential run: if the rules this transcoder reads out of the
// source are identical to the ones upstream's resolver actually uses, then a
// matcher already verified against those rules resolves identically over
// these. A divergence here is a transcoder bug, and it is fatal -- shipping
// definitions whose match rules differ from upstream's means devices resolving
// to the wrong definition, silently.
// ---------------------------------------------------------------------------
if (harvested && fs.existsSync(harvested)) {
  const want = new Map();
  for (const d of JSON.parse(fs.readFileSync(harvested, 'utf8'))) {
    want.set(d.m, (d.z ?? []).slice().sort());
  }
  let checked = 0;
  const wrong = [];
  for (const ir of irs) {
    const expected = want.get(ir.model);
    if (!expected) continue;
    checked += 1;
    const got = ir.match.models.slice().sort();
    if (JSON.stringify(got) !== JSON.stringify(expected)) {
      wrong.push({model: ir.model, got, expected});
    }
  }
  console.log(`\n=== match-rule cross-validation against upstream's runtime ===`);
  console.log(`  compared: ${checked}, divergent: ${wrong.length}`);
  for (const w of wrong.slice(0, 10)) {
    console.log(`  ! ${w.model}: got ${JSON.stringify(w.got)} expected ${JSON.stringify(w.expected)}`);
  }
  if (wrong.length > 0) {
    console.error(`\nFATAL: ${wrong.length} definitions have match rules that differ from upstream.`);
    process.exit(1);
  }
}

// ---------------------------------------------------------------------------
// COVERAGE.md -- the project's device-support KPI.
// ---------------------------------------------------------------------------
const pct = (n) => `${(100 * n / total).toFixed(1)}%`;
const row = ([name, e]) => `| \`${name}\` | ${e.kind} | ${e.soleBlocker} | ${e.devices} |`;
const md = `# Device coverage

Generated by \`scripts/transcode-devices.mjs\` from zigbee-herdsman-converters.
**Do not edit by hand**; re-run \`scripts/refresh-device-coverage.sh\`.

Upstream definitions read: **${total}**.

## Where they stand

| state | count | share | meaning |
|---|---:|---:|---|
| complete | ${byClass.get('complete') ?? 0} | ${pct(byClass.get('complete') ?? 0)} | fully expressed as data |
| approximate | ${byClass.get('approximate') ?? 0} | ${pct(byClass.get('approximate') ?? 0)} | works, with something named not expressed |
| needs-primitive | ${byClass.get('needs-primitive') ?? 0} | ${pct(byClass.get('needs-primitive') ?? 0)} | blocked only on named shared helpers |
| needs-rust | ${byClass.get('needs-rust') ?? 0} | ${pct(byClass.get('needs-rust') ?? 0)} | blocked on per-device code |
| unsupported | ${byClass.get('unsupported') ?? 0} | ${pct(byClass.get('unsupported') ?? 0)} | upstream's own deprecated path |

**Usable today: ${usable} / ${total} = ${pct(usable)}.**

\`complete\` and \`approximate\` are reported separately and never merged. An
approximation is a device that works with something missing — a Hue bulb whose
gradient effects are not expressed is still a working light, but calling that
"complete" is how a coverage number becomes a lie.

## What to implement next

Ranked by **sole** — definitions where this is the *only* remaining blocker, so
implementing it moves them straight to usable. \`total\` counts every definition
that mentions it, which is the eventual reach.

| primitive | kind | sole | total |
|---|---|---:|---:|
${ranked.slice(0, 30).map(row).join('\n')}

Distinct missing primitives: **${blockedBy.size}**, so the tail is long and the
top of this table is where the leverage is.

## What this number is not

It is produced by reading upstream's TypeScript with the compiler's own parser,
not by running its converters. It says what can be *expressed* as data, not that
any device has been tested. A definition marked complete has never touched
hardware.
`;
fs.writeFileSync('COVERAGE.md', md);

// Every `Extend` variant this transcoder claims it can emit. A Rust test
// checks each one exists, so adding a name here without adding the variant
// fails the build rather than inflating the coverage number.
fs.writeFileSync(
  'claimed-primitives.json',
  JSON.stringify({
    extends: [...new Set(KNOWN_EXTENDS.values())].sort(),
    tuyaConverterKinds: [...new Set([...KNOWN_TUYA_CONVERTERS.values()].map((v) => v.kind))].sort(),
    // Cluster ids this transcoder resolves names to. A wrong number here
    // produces a binding to whatever cluster happens to live at it, so the
    // Rust side checks every one against the cluster registry.
    clusters: Object.fromEntries([...CLUSTER_IDS].sort((a, b) => a[1] - b[1])),
  }, null, 1),
);
console.log('wrote COVERAGE.md and claimed-primitives.json');
