//! Parameters for the  [`Security::import_transient_key`](crate::Security::import_transient_key) command.
//!
//! # Local patch
//!
//! Upstream 17.0.0 declares a leading `context: Context` field here. EZSP v13's
//! `importTransientKey` does not carry one: its payload is `eui64` (8) +
//! `plaintextKey` (16) + `flags` (1) = 25 bytes, and its response is a single
//! `sl_status_t`.
//!
//! Measured against EmberZNet 7.4.4 rather than read off a datasheet. A
//! reference stack's frame for this command is 30 bytes on the wire; ours was
//! 47. With the EZSP v13 header at 5 bytes -- cross-checked against
//! `SET_POLICY` (7), `SET_CONFIGURATION_VALUE` (8) and `PERMIT_JOINING` (6) --
//! that is 25 bytes of payload against our 42, the difference being exactly
//! `Context`'s 17.
//!
//! The consequence is worse than a rejected frame: the NCP parses the first 25
//! bytes and answers `OK`, so an EUI64 taken from the context's leading bytes
//! and a key spliced across the context and eui64 fields are installed
//! silently. A Zigbee 3.0 device then joins and cannot finish commissioning,
//! rejoining every few seconds, while every call in the log looks successful.
//!
//! `Command::new` keeps its four-argument shape so the `Security` trait needs
//! no change; the context is accepted and discarded.

use num_traits::FromPrimitive;
use silizium::Status;
use silizium::zigbee::security::man::{Context, Flags, Key};

use crate::Error;
use crate::ember::Eui64;

crate::frame::parameters::frame!(
    0x0111,
    { eui64: Eui64, plaintext_key: Key, flags: u8 },
    impl {
        impl Command {
            /// Creates command parameters.
            ///
            /// `_context` is ignored: the EZSP command has no context field.
            /// It stays in the signature so callers and the `Security` trait
            /// are unaffected by this patch.
            #[must_use]
            pub fn new(_context: Context, eui64: Eui64, plaintext_key: Key, flags: Flags) -> Self {
                Self {
                    eui64,
                    plaintext_key,
                    flags: flags.bits(),
                }
            }
        }
    },
    { status: u32 } => Security(security)::ImportTransientKey,
    impl {
        /// Convert the response into `()` or an appropriate [`Error`] depending on its status.
        impl TryFrom<Response> for () {
            type Error = Error;

            fn try_from(response: Response) -> Result<Self, Self::Error> {
                match Status::from_u32(response.status).ok_or(response.status) {
                    Ok(Status::Ok) => Ok(()),
                    other => Err(other.into()),
                }
            }
        }
    }
);
