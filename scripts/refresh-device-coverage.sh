#!/usr/bin/env bash
# Regenerates COVERAGE.md and the claimed-primitives fixture by transcoding
# zigbee-herdsman-converters.
#
# The whole pipeline, in the order that makes the number trustworthy:
#
#   extract      read upstream's TypeScript with the compiler's own parser
#   transcode    emit the declarative IR
#   validate     cross-check extracted match rules against upstream's runtime
#   emit         turn the IR into the generated Rust the crate ships
#   verify       `cargo test` checks the claimed primitives really exist, the
#                differential test checks resolution still agrees, and the
#                bundled test checks real devices actually resolve
#
# Only a number that survives all four belongs in COVERAGE.md. Needs node, npm
# and git; nothing in the build or the test suite does.
set -euo pipefail

VERSION="${1:-26.104.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURES="$ROOT/crates/rszigbee-devices/tests/fixtures"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for tool in node npm git; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done

echo "==> fetching zigbee-herdsman-converters $VERSION source"
git clone --depth 1 --quiet --branch "v$VERSION" \
  https://github.com/Koenkk/zigbee-herdsman-converters.git "$WORK/zhc" 2>/dev/null \
  || git clone --depth 1 --quiet \
       https://github.com/Koenkk/zigbee-herdsman-converters.git "$WORK/zhc"

echo "==> installing the TypeScript parser and herdsman"
cd "$WORK"
npm init -y >/dev/null 2>&1
# zigbee-herdsman as well as typescript: the transcoder imports its `Zcl`
# table to resolve cluster and attribute names to ids. Without it the whole
# pipeline failed at the first import, so this script could not be run at all.
npm install typescript@5 zigbee-herdsman --no-audit --no-fund --silent

echo "==> transcoding"
cp "$ROOT/scripts/transcode-devices.mjs" .
# The harvested match rules are the cross-validation oracle: the transcoder
# exits non-zero if what it reads out of the source disagrees with what
# upstream's own resolver uses.
node transcode-devices.mjs "$WORK/zhc/src/devices" "$FIXTURES/match-rules.json"

cp COVERAGE.md "$ROOT/COVERAGE.md"
cp claimed-primitives.json "$FIXTURES/claimed-primitives.json"
echo "==> wrote COVERAGE.md and $FIXTURES/claimed-primitives.json"

# The IR is not the deliverable; the generated Rust is. Match rules come from
# the harvested fixture rather than the IR because source extraction misses
# 720 definitions whose rules are not literals -- see emit-definitions.mjs.
echo "==> emitting generated Rust definitions"
cp "$ROOT/scripts/emit-definitions.mjs" .
node emit-definitions.mjs definitions.json "$FIXTURES/match-rules.json" \
  custom-clusters.json \
  > "$ROOT/crates/rszigbee-devices/src/generated.rs"

# Formatted here, not left to the author. A generator's output will not match
# rustfmt by accident, and `cargo fmt --check` is a CI gate -- forgetting this
# step put a formatting-only failure on main twice.
echo "==> formatting the generated definitions"
cd "$ROOT"
cargo fmt -p rszigbee-devices

echo "==> verifying"
cd "$ROOT"
cargo test -p rszigbee-devices --all-features
