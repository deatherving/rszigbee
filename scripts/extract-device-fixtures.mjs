// Two outputs, taken from zhc's own runtime rather than from reading its source:
//   fixtures.json  -- match rules for every definition
//   expected.json  -- what zhc's findByDevice actually answers for each probe
import fs from 'node:fs';
import * as zhc from 'zigbee-herdsman-converters';

zhc.setLogger({debug() {}, info() {}, warning() {}, error() {}});

const dir = 'node_modules/zigbee-herdsman-converters/dist/devices';
const files = fs.readdirSync(dir).filter((f) => f.endsWith('.js')).sort();

const defs = [];
for (const f of files) {
  try {
    const mod = await import('./' + dir + '/' + f);
    const arr = Array.isArray(mod.definitions) ? mod.definitions : mod.default?.definitions;
    if (Array.isArray(arr)) for (const d of arr) defs.push(d);
  } catch {}
}

// ---- fixtures: only the fields matching depends on.
const FP = ['modelID', 'manufacturerName', 'manufacturerID', 'applicationVersion', 'stackVersion',
            'zclVersion', 'hardwareVersion', 'dateCode', 'softwareBuildID', 'powerSource', 'type', 'priority'];
const fixtures = defs.map((d) => ({
  model: d.model,
  vendor: d.vendor ?? '',
  models: d.zigbeeModel ?? [],
  fingerprints: (d.fingerprint ?? []).map((fp) => {
    const out = {};
    for (const k of FP) if (fp[k] !== undefined) out[k] = fp[k];
    if (fp.ieeeAddr) out.ieeeAddr = String(fp.ieeeAddr);
    if (fp.endpoints) {
      out.endpoints = fp.endpoints.map((e) => ({
        ID: e.ID, deviceID: e.deviceID, profileID: e.profileID,
        inputClusters: e.inputClusters, outputClusters: e.outputClusters,
      }));
    }
    return out;
  }),
  whiteLabels: (d.whiteLabel ?? []).filter((w) => 'fingerprint' in w).map((w) => ({
    model: w.model,
    vendor: w.vendor ?? null,
    description: w.description ?? null,
    fingerprints: w.fingerprint.map((fp) => {
      const out = {};
      for (const k of FP) if (fp[k] !== undefined) out[k] = fp[k];
      if (fp.ieeeAddr) out.ieeeAddr = String(fp.ieeeAddr);
      if (fp.endpoints) out.endpoints = true;
      return out;
    }),
  })),
}));
fs.writeFileSync('fixtures.json', JSON.stringify(fixtures));
console.log('fixtures: ' + fixtures.length + ' definitions');

// ---- probes: every distinct (modelID, manufacturerName) pair upstream knows,
// which is the input space that actually matters, plus deliberate misses.
const probes = new Map();
const add = (modelID, manufacturerName) => {
  const key = modelID + '  ' + (manufacturerName ?? '');
  if (!probes.has(key)) probes.set(key, {modelID, manufacturerName: manufacturerName ?? undefined});
};
for (const d of defs) {
  for (const m of d.zigbeeModel ?? []) add(m, undefined);
  for (const fp of d.fingerprint ?? []) {
    // Skip fingerprints needing a richer stub than we build below; those are
    // covered by the unit tests instead.
    if (fp.endpoints || fp.ieeeAddr || fp.applicationVersion !== undefined) continue;
    add(fp.modelID ?? 'TS0601', fp.manufacturerName);
  }
}
add('definitely-not-a-real-model', 'nobody');
add('TS0601', '_TZE200_this_does_not_exist');

function stub({modelID, manufacturerName}) {
  const eps = [{ID: 1, deviceID: 0x0100, profileID: 0x0104, inputClusters: [0, 6], outputClusters: []}];
  return {
    type: 'EndDevice', ieeeAddr: '0x00124b0022189abc',
    modelID, manufacturerName, endpoints: eps,
    getEndpoint: (id) => eps.find((e) => e.ID === id),
    interviewCompleted: true, interviewing: false, meta: {}, save() {},
  };
}

const expected = [];
for (const probe of probes.values()) {
  let base = null;
  let branded = null;
  try {
    const raw = await zhc.findDefinition(stub(probe));
    base = raw?.model ?? null;
    const prepared = await zhc.findByDevice(stub(probe));
    branded = prepared?.model ?? null;
  } catch {
    base = '(threw)';
    branded = '(threw)';
  }
  expected.push({...probe, base, branded});
}
fs.writeFileSync('expected.json', JSON.stringify(expected));
const matched = expected.filter((e) => e.base && e.base !== '(threw)').length;
const threw = expected.filter((e) => e.base === '(threw)').length;
const renamed = expected.filter((e) => e.base && e.branded && e.base !== e.branded).length;
console.log('probes where a white label renames the result: ' + renamed);
console.log('probes: ' + expected.length + ', upstream matched: ' + matched +
            ', no match: ' + (expected.length - matched - threw) + ', threw: ' + threw);
