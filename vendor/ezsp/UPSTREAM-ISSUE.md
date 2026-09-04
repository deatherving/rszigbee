# Ready-to-file bug report for github.com/PaulmannLighting/ezsp

Not filed. Posting this publicly is the maintainer-facing step, so it is left
for a human to send. Everything below is verified against real firmware; see
`README.md` in this directory for the local patch it describes.

When it is filed and released, delete `vendor/ezsp`, the `[patch.crates-io]`
entry in the workspace `Cargo.toml`, and this file.

---

**Title:** `importTransientKey` sends a `Context` field EZSP v13 does not have (frame is 47 bytes, should be 30)

**Body:**

`Security::import_transient_key` builds frame `0x0111` with a leading
`context: Context` field:

```rust
// src/frame/parameters/security/import_transient_key.rs
crate::frame::parameters::frame!(
    0x0111,
    { context: Context, eui64: Eui64, plaintext_key: Key, flags: u8 },
```

EZSP v13's `importTransientKey` takes only `eui64` (8) + `plaintextKey` (16) +
`flags` (1) = 25 bytes of payload, and returns a single `sl_status_t`. The
`Context` adds 17 bytes that the NCP does not expect.

### Evidence

Captured from `ashv2`'s own `Sending EZSP frame (bytes)` trace against a Sonoff
ZBDongle-E (EFR32MG21, EmberZNet 7.4.4 GA, EZSP v13). The frame this crate
sends is 47 bytes:

```
16 00 01 11 01 03 00 00 ff ff ff ff ff ff ff ff 00 00 00 00 00 00
ff ff ff ff ff ff ff ff 5a 69 67 42 65 65 41 6c 6c 69 61 6e 63 65 30 39 00
seq fc--- id--- <------------- Context, 17 bytes ------------->
                                        eui64 (8)           ZigBeeAlliance09 (16)  flags
```

A reference stack (zigbee-herdsman's ember driver) sends **30** bytes for the
same command on the same firmware, logged as
`===> [FRAME: ID=273:"IMPORT_TRANSIENT_KEY" Seq=45 Len=30]`, with the response
`Len=9`.

The 5-byte EZSP v13 header is cross-checked against three commands whose
payloads are unambiguous:

| command | logged length | header + payload |
|---|---:|---|
| `SET_POLICY` | 7 | 5 + policyId(1) + decisionId(1) |
| `SET_CONFIGURATION_VALUE` | 8 | 5 + configId(1) + value(2) |
| `PERMIT_JOINING` | 6 | 5 + duration(1) |
| `IMPORT_TRANSIENT_KEY` | 30 | 5 + eui64(8) + key(16) + flags(1) |

So 30 − 5 = 25, and 47 − 5 = 42 = 25 + `Context`'s 17.

### Why this is worse than a rejected frame

The NCP parses the first 25 bytes and answers `OK`. With the frame above it
therefore imports:

- `eui64` = `03 00 00 ff ff ff ff ff` — the context's `core_key_type`,
  `key_index`, `derived_type` and the first five bytes of its own eui64
- `plaintext_key` = 16 bytes spliced across the rest of the context and the
  real `eui64` argument
- `flags` = `0xff`

Nothing reports a problem. The observable consequence is that a Zigbee 3.0
device joins, cannot complete commissioning against a key the coordinator does
not actually hold, and rejoins every few seconds indefinitely — while every
call in the log looks successful. A SONOFF SWV-ZNU valve rejoined every 6–30
seconds before the fix and joined exactly once after it.

### Suggested fix

Drop the field from the frame:

```rust
crate::frame::parameters::frame!(
    0x0111,
    { eui64: Eui64, plaintext_key: Key, flags: u8 },
```

`Command::new` can keep its four-argument shape and discard the context, which
leaves the `Security` trait and all callers unchanged — that is what the local
patch does, to keep the diff to one file.

Worth checking the sibling security-manager commands (`export_transient_key`,
`import_key`, `export_key`) against the same length arithmetic; only
`import_transient_key` was needed here, so only it was verified.
