# Vendored `ezsp` 17.0.0, with one file changed

Upstream: <https://crates.io/crates/ezsp> — MIT. This is release 17.0.0
unmodified except for the single fix below, kept here so the build is
reproducible without depending on a fork being reachable.

**Delete this directory and the `[patch.crates-io]` entry in the workspace
`Cargo.toml` as soon as a release carries the fix.** 17.0.0 was the newest
release when this was written, so there was nothing to upgrade to.

## The change

`src/frame/parameters/security/import_transient_key.rs` declared a leading
`context: Context` field. EZSP v13's `importTransientKey` (frame `0x0111`) has
no such field: its payload is

```
eui64 (8) + plaintextKey (16) + flags (1) = 25 bytes
```

`Command::new` keeps its four-argument shape, so the `Security` trait and every
caller are unaffected; the context argument is accepted and discarded.

## How it was found, and why it matters

Not from a datasheet — by comparing our frames against a reference stack on the
same dongle and firmware (EmberZNet 7.4.4, EZSP 13).

The reference's frame for this command is **30 bytes** on the wire. Ours was
**47**. The EZSP v13 header is 5 bytes, cross-checked three ways against
commands whose payloads are known:

| command | logged length | header + payload |
|---|---|---|
| `SET_POLICY` | 7 | 5 + policyId(1) + decisionId(1) |
| `SET_CONFIGURATION_VALUE` | 8 | 5 + configId(1) + value(2) |
| `PERMIT_JOINING` | 6 | 5 + duration(1) |
| `IMPORT_TRANSIENT_KEY` | 30 | 5 + eui64(8) + key(16) + flags(1) |

So 25 bytes of payload against our 42 — a difference of exactly `Context`'s 17.

The failure mode is worse than a rejected frame. The NCP reads the first 25
bytes and answers `OK`, so it installs an EUI64 taken from the context's
leading bytes and a key spliced across the context and `eui64` fields. Captured
before the fix:

```
16 00 01 11 01 03 00 00 ff ff ff ff ff ff ff ff 00 00 00 00 00 00
ff ff ff ff ff ff ff ff 5a 69 67 42 65 65 41 6c 6c 69 61 6e 63 65 30 39 00
                        ^-- ZigBeeAlliance09, 17 bytes further into the frame
                            than the NCP looks for it
```

and after:

```
16 00 01 11 01 ff ff ff ff ff ff ff ff 5a 69 67 42 65 65 41 6c 6c 69 61 6e 63 65 30 39 00
              |---------- eui64 ----------|-------- ZigBeeAlliance09 --------|flags
```

Consequence on hardware: a Zigbee 3.0 device joins, cannot complete
commissioning against a key the coordinator does not actually hold, and
rejoins every few seconds forever — while every call in the log reports
success. A SONOFF SWV-ZNU valve rejoined every 6–30 seconds before this fix and
joined exactly once after it, then began reporting battery and on/off state
normally.
