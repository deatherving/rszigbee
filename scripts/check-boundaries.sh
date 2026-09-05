#!/usr/bin/env bash
# Enforces the dependency rules from the README, "Boundaries". These are
# the rules that keep MQTT out of the core; a comment saying "must not depend
# on MQTT" is worth nothing without a check that fails the build.
set -euo pipefail

fail=0
# All diagnostics go to stderr: `tree` is called inside a command
# substitution, and anything it writes to stdout would be captured into the
# caller's variable instead of shown.
note() { printf '  %s\n' "$*" >&2; }
bad()  { printf 'FAIL: %s\n' "$*" >&2; fail=1; }
ok()   { printf 'ok:   %s\n' "$*" >&2; }

# A dependency check that silently sees an empty tree passes vacuously, which
# is worse than no check at all. Fail loudly instead.
# Default features only. The rule is that serialisation lives at system
# boundaries, not that JSON is banned outright: `rszigbee-core/file-store`
# legitimately pulls serde_json to write to disk. Checking with --all-features
# would conflate "core parses JSON in its data path" with "core can persist",
# and the first is the thing worth preventing.
tree() {
  local out
  if ! out=$(cargo tree --quiet -p "$1" --edges normal --prefix none 2>&1); then
    bad "cannot resolve the dependency tree for '$1'"
    note "cargo tree said: $(head -2 <<<"$out" | tr '\n' ' ')"
    note "Every boundary check below would pass vacuously, so stopping."
    exit 1
  fi
  if [ -z "$out" ]; then
    bad "the dependency tree for '$1' is empty; the checks would pass vacuously"
    exit 1
  fi
  sort -u <<<"$out"
}

# --- rszigbee-core must not reach MQTT or Home Assistant ---
core="$(tree rszigbee-core)"
for forbidden in rumqttc rumqttd mqtt; do
  if grep -qi "^${forbidden} " <<<"$core"; then
    bad "rszigbee-core depends on '${forbidden}'"
    note 'MQTT belongs in rszigbee-mqtt. See README, "Boundaries".'
  fi
done

# serde_json must stay out of the DEFAULT graph. The rule is that serialisation
# lives at system boundaries, not that JSON is banned: rszigbee-core/file-store
# legitimately pulls it to write to disk. What must never happen is JSON on the
# internal data path, or a caller using only MemoryStore linking a JSON parser.
if grep -qi '^serde_json ' <<<"$core"; then
  bad "rszigbee-core pulls serde_json with default features"
  note 'Persistence formats belong behind the "file-store" feature.'
fi

if grep -qi 'homeassistant\|home-assistant' <<<"$core"; then
  bad "rszigbee-core depends on a Home Assistant crate"
fi
# Only claim the section passed if nothing in it failed, or the summary
# contradicts the failure printed just above it.
[ "$fail" -eq 0 ] && ok "rszigbee-core is free of MQTT, Home Assistant and default-on JSON"

# --- EZSP must be contained in the Ember adapter ---
# The rule the architecture rests on: core and the adapter trait do not know
# EZSP exists. Only rszigbee-adapter-ember does.
for crate in rszigbee-core rszigbee-adapter rszigbee-spec; do
  t="$(tree "$crate")"
  for forbidden in rsezsp ezsp ashv2 tokio-serial serialport; do
    if grep -qi "^${forbidden} " <<<"$t"; then
      bad "$crate depends on '${forbidden}'"
      note "Coordinator protocols belong in rszigbee-adapter-<family> only."
    fi
  done
done
grep -qiE '^(rsezsp|ezsp|ashv2) ' <<<"$(tree rszigbee-core)$(tree rszigbee-adapter)$(tree rszigbee-spec)" \
  || ok "EZSP is contained in the Ember adapter"

# --- the facade must expose every internal crate a user needs ---
if [ -d crates/rszigbee ]; then
  facade="$(tree rszigbee)"
  # Tracked rather than reported per crate, so a failure does not print
  # alongside an "ok" for the same check and leave the reader guessing.
  missing=0
  for needed in rszigbee-core rszigbee-adapter rszigbee-spec rszigbee-devices; do
    if ! grep -q "^${needed} " <<<"$facade"; then
      bad "the rszigbee facade does not re-export ${needed}"
      missing=1
    fi
  done
  [ "$missing" -eq 0 ] && ok "the facade covers the internal crates"
fi

# --- rszigbee-spec is sans-IO ---
spec="$(tree rszigbee-spec)"
for forbidden in tokio serialport tokio-serial mio socket2; do
  if grep -qi "^${forbidden} " <<<"$spec"; then
    bad "rszigbee-spec depends on '${forbidden}'"
    note "rszigbee-spec is sans-IO: codecs and data only."
  fi
done
grep -qiE '^(tokio|serialport|tokio-serial|mio|socket2) ' <<<"$spec" \
  || ok "rszigbee-spec is sans-IO"

# --- rszigbee-mqtt is sans-IO too ---
#
# The MQTT crate holds the contract, not a client. That is what lets the exact
# topics and payloads be tested against captured ones with no broker running,
# and it keeps the choice of MQTT library out of the part that has to be byte
# for byte right. An MQTT client or a runtime dependency appearing here would
# take that away quietly, so it is checked rather than trusted.
if [ -d crates/rszigbee-mqtt ]; then
  mqtt_io=0

  # Its *own* dependencies, read from the manifest rather than from
  # `cargo tree`. The tree is the wrong instrument here: this crate depends on
  # rszigbee-core, which legitimately owns the tokio task that drives the
  # radio, so tokio appears transitively and always will. What must not appear
  # is tokio as a *direct* dependency of this crate.
  direct="$(awk '/^\[dependencies\]/{f=1;next} /^\[/{f=0} f' crates/rszigbee-mqtt/Cargo.toml)"
  for forbidden in tokio mio socket2; do
    if grep -qE "^${forbidden}[ .=]" <<<"$direct"; then
      bad "rszigbee-mqtt depends directly on '${forbidden}'"
      note "rszigbee-mqtt is the contract, not a client: events to publications"
      note "and messages to intents, so it is testable without a broker."
      mqtt_io=1
    fi
  done

  # An MQTT client must not appear anywhere in its tree, transitively or not.
  # Nothing it depends on has any business pulling one in.
  mqtt_tree="$(tree rszigbee-mqtt)"
  for forbidden in rumqttc paho-mqtt mqtt-async-client ntex-mqtt mqtt-protocol; do
    if grep -qi "^${forbidden} " <<<"$mqtt_tree"; then
      bad "rszigbee-mqtt pulls in the MQTT client '${forbidden}'"
      note "The contract crate holds no client, so the topics and payloads can"
      note "be tested against captured ones with no broker running."
      mqtt_io=1
    fi
  done

  [ "$mqtt_io" -eq 0 ] && ok "rszigbee-mqtt is sans-IO"
fi

# --- rszigbee-devices must not depend on the runtime (once it exists) ---
if [ -d crates/rszigbee-devices ]; then
  if tree rszigbee-devices | grep -q '^rszigbee-core '; then
    bad "rszigbee-devices depends on rszigbee-core"
    note "Definitions are data the runtime interprets, not the reverse."
    note "This rule is what makes the importer and validator possible."
  else
    ok "rszigbee-devices does not depend on rszigbee-core"
  fi
fi

# --- ZigbeeStore must stay a Zigbee-domain trait ---
# The generic blob escape hatch was removed in report r2; this stops it coming
# back through the side door.
if awk '/pub trait ZigbeeStore/,/^}/' crates/rszigbee-core/src/store.rs \
     | grep -qiE 'blob|mqtt|homeassistant|discovery'; then
  bad "ZigbeeStore has gained a blob/MQTT/Home Assistant method"
  note "Layers above core own their own persistence. See the README design notes."
else
  ok "ZigbeeStore holds only Zigbee domain state"
fi

# --- no unsafe ---
if grep -rn --include='*.rs' -E '^\s*unsafe\b' crates/ | grep -v '^\s*//'; then
  bad "unsafe code found; it must be justified in an ADR first"
else
  ok "no unsafe code"
fi

exit "$fail"
