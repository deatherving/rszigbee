#!/usr/bin/env bash
# Regenerates the ZCL cluster table from zigbee-herdsman.
#
# The table is read from herdsman's *runtime* definitions rather than parsed
# out of its TypeScript, so the ids and types are the ones it actually uses.
# Needs node and npm; nothing in the build or the test suite does.
set -euo pipefail

VERSION="${1:-latest}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/crates/rszigbee-spec/src/zcl/generated.rs"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for tool in node npm; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done

echo "==> installing zigbee-herdsman@$VERSION"
cd "$WORK"
npm init -y >/dev/null 2>&1
npm install "zigbee-herdsman@$VERSION" --no-audit --no-fund --silent

echo "==> transcoding"
cp "$ROOT/scripts/transcode-clusters.mjs" .
node transcode-clusters.mjs > "$OUT"

cd "$ROOT"
cargo fmt -p rszigbee-spec
echo "==> verifying"
# The tests check the table's scale, the ids the runtime resolves by name, that
# attribute types survive, and that composite-parameter commands are marked
# unencodable rather than emitted short.
cargo test -p rszigbee-spec --all-features
cargo test -p rszigbee-devices --all-features --test claimed_primitives
