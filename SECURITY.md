# Security

## Reporting

Report vulnerabilities through GitHub's private advisory form on this
repository: **Security → Report a vulnerability**. Please do not open a public
issue for anything exploitable.

There is no formal response-time commitment. rszigbee is early-stage and
maintained without funding; expect best effort, and say in the report if you
have a disclosure deadline.

## Scope

rszigbee parses frames from a radio. Every device within range is an untrusted
input source, including devices that never joined the network. In scope:

- A crash, panic or hang reachable from a received frame, a device-reported
  string, or a device-supplied length or count.
- A network key, link key or frame counter reaching a log, an error message, a
  `Debug` output or a file readable by another user.
- Persistence accepting a state that silently destroys a network — the case that
  matters most is forming a new network when an existing one should have been
  resumed, which orphans every joined device irrecoverably.
- Frame-counter or replay handling that weakens Zigbee's own protections.

Out of scope: weaknesses in the Zigbee specification itself (the well-known
`ZigBeeAlliance09` link key among them), physical attacks on a coordinator, and
anything in a dependency — report those to the dependency.

## What the code already assumes

These are treated as invariants, so a counterexample to any of them is a bug
worth reporting:

- **No parse path panics.** Decoders return `Result`; slice indexing, `unwrap`,
  `expect` and `panic!` are denied by clippy outside tests.
- **Keys are not printable.** `SecretKey`'s `Debug` prints `SecretKey([redacted])`
  and the key material is reachable only through an explicit `expose()`.
- **Forming requires opting in.** `MismatchPolicy::Fail` is the default, so a
  coordinator whose network does not match the stored one is a refusal.
- **Key generation fails closed.** Network keys come from the OS CSPRNG; if it
  is unavailable, forming fails rather than proceeding with a weak key.
- **A corrupt network file stops startup.** Continuing would mean forming.

## Operator notes

The store holds the network key in plaintext, at mode 0600 in a directory the
process creates. It is worth exactly as much as the network: anyone who reads it
can join, decrypt and impersonate. Give it a dedicated user, and treat backups
of it as key material.
