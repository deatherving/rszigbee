#!/usr/bin/env bash
# Regenerates the differential-test fixtures from zigbee-herdsman-converters.
#
# The fixtures are not hand-written: they are what upstream's own resolver
# answers. That is the whole point -- reading upstream's algorithm and
# reimplementing it is not evidence the two agree, and this produces the
# evidence. Re-run it when bumping the pinned upstream version.
#
# Needs node and npm. Nothing in the build or the test needs them; only this.
set -euo pipefail

VERSION="${1:-26.104.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DST="$ROOT/crates/rszigbee-devices/tests/fixtures"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

command -v node >/dev/null || { echo "node is required" >&2; exit 1; }
command -v npm >/dev/null || { echo "npm is required" >&2; exit 1; }

echo "==> installing zigbee-herdsman-converters@$VERSION"
cd "$WORK"
npm init -y >/dev/null 2>&1
npm install "zigbee-herdsman-converters@$VERSION" --no-audit --no-fund --silent

cp "$ROOT/scripts/extract-device-fixtures.mjs" extract.mjs
echo "==> extracting"
node extract.mjs

python3 - "$WORK" "$DST" <<'PY'
import json, pathlib, sys
work, dst = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
dst.mkdir(parents=True, exist_ok=True)

slim = []
for d in json.load(open(work / "fixtures.json")):
    if not d["models"] and not d["fingerprints"]:
        continue
    e = {"m": d["model"]}
    if d["models"]:
        e["z"] = d["models"]
    if d["fingerprints"]:
        e["f"] = d["fingerprints"]
    if d["whiteLabels"]:
        e["w"] = d["whiteLabels"]
    slim.append(e)
json.dump(slim, open(dst / "match-rules.json", "w"), separators=(",", ":"))

probes = json.load(open(work / "expected.json"))
json.dump(
    [[p["modelID"], p.get("manufacturerName"), p["base"], p["branded"]] for p in probes],
    open(dst / "expected.json", "w"),
    separators=(",", ":"),
)
print(f"==> {len(slim)} definitions, {len(probes)} probes -> {dst}")
PY

echo "==> verifying"
cd "$ROOT"
cargo test -p rszigbee-devices --all-features --test upstream_differential -- --nocapture
