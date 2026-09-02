# Contributing to rszigbee

## The short version

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
./scripts/check-boundaries.sh
```

All four have to pass. None of them need Zigbee hardware.

## Device support belongs upstream first

If a device does not work, the fix almost always belongs in
[zigbee-herdsman-converters](https://github.com/Koenkk/zigbee-herdsman-converters),
not here. rszigbee's device data originates there, so a fix upstream reaches
Zigbee2MQTT, Home Assistant and rszigbee users; a fix only here reaches one of
them and then drifts.

Open an issue here when the device already works upstream but not in rszigbee —
that is a bug in our definition pipeline, and it is ours to fix.

## What a change needs

**A test that would have failed before it.** For a decoder, that means a frame;
for a policy, the input that produced the wrong decision. "Cannot panic" claims
need a test that feeds the truncated input.

**A reason in the code, not in the commit message.** The comments that matter
here explain *why* a value is what it is — which device broke without it, which
specification paragraph requires it, what the observed failure was. Commit
messages are not read by the next person editing the line.

**Attribution when data comes from somewhere.** Cluster lists, attribute ids and
interview quirks transcoded from zigbee-herdsman carry a comment naming the
upstream file. Do not copy or translate code from a GPL-3.0 project — see
[`ATTRIBUTION.md`](ATTRIBUTION.md).

## The boundaries are not style preferences

`scripts/check-boundaries.sh` enforces three rules mechanically:

1. `rszigbee-core` does not depend on `ezsp`, `ashv2` or a serial port.
2. `rszigbee-core` has no MQTT, and no JSON in its default features.
3. `rszigbee-spec` has no tokio, no serial and no I/O.

Each has a negative control, so the check is known to fail when the rule is
broken. If a change needs a boundary moved, that is a design discussion — open
an issue rather than adding an exception.

## Parse paths never panic

Anything decoding a radio frame, a device-reported string or a length prefix is
handling untrusted input. In `rszigbee-spec` and every decoder, slice indexing,
`unwrap`, `expect` and `panic!` are denied by clippy, relaxed only in `#[cfg(test)]`.
A device claiming 200 endpoints while sending two must produce a typed error.

## Hardware changes

Anything touching `rszigbee-adapter-ember` needs a run against real hardware:

```sh
cargo run -p rszigbee --example ember_selftest -- /dev/ttyUSB0
```

Paste the output in the pull request. `--form` writes a new network key to the
dongle and orphans anything joined to it, so only use it on a blank one.

`spikes/ezsp-probe` is read-only and safe against a live network.

## Licence of contributions

Contributions are taken under `MIT OR Apache-2.0`, matching the workspace. The
licensing position on the future MQTT layer is not final — see the README.
