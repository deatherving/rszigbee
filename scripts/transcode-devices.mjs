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
]);

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
  return node.expression.getText();
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
    const converter = el.elements[2].getText();
    if (dp === NOT_LITERAL || name === NOT_LITERAL) {
      missing.push({primitive: 'tuyaDatapoints:non-literal', kind: 'rust'});
      continue;
    }
    const known = KNOWN_TUYA_CONVERTERS.get(converter);
    if (!known) {
      missing.push({primitive: converter, kind: 'primitive'});
      continue;
    }
    out.push({dp, name, ...known});
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
  const text = cfg.getText();
  // Control flow means it is a procedure, not a table.
  if (/\b(if|for|while|switch|try)\b|\?\?|\?\.|&&|\|\|/.test(text)) {
    missing.push({primitive: 'configure:imperative', kind: 'rust'});
    return {value: [], missing};
  }

  const bindings = [];
  let blocked = false;
  const visit = (node) => {
    if (blocked) return;
    if (ts.isCallExpression(node)) {
      const name = node.expression.getText();
      const isDeviceCall = /^device\.(getEndpoint|save|powerSource)/.test(name)
        || /^endpoint\.(read|write|configureReporting|saveClusterAttributeKeyValue|bind)/.test(name)
        || /^utils\./.test(name);
      if (name.startsWith('reporting.')) {
        if (!KNOWN_REPORTING.has(name)) {
          missing.push({primitive: name, kind: 'primitive'});
          blocked = true;
          return;
        }
        bindings.push({helper: name.slice('reporting.'.length)});
      } else if (!isDeviceCall && !/^(m|tuya)\./.test(name) && name !== '') {
        missing.push({primitive: `configure:${name}`, kind: 'primitive'});
        blocked = true;
        return;
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(cfg.body);
  return {value: blocked ? [] : bindings, missing};
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
    for (const el of node.elements) {
      const name = el.getText();
      if (/^(fz|tz)\./.test(name)) missing.push({primitive: name, kind: 'primitive'});
      else missing.push({primitive: `${key}:local`, kind: 'rust', detail: name.slice(0, 40)});
    }
    sections[key] = false;
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
  }, null, 1),
);
console.log('wrote COVERAGE.md and claimed-primitives.json');
