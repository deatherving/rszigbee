//! The `Zigbee2MQTT`-compatible MQTT contract, as data.
//!
//! rszigbee's primary path is the typed Rust API; this is the optional second
//! mode, so that something already speaking `Zigbee2MQTT` — Home Assistant, Node
//! RED, a shell script — keeps working against this stack.
//!
//! # Where this contract comes from
//!
//! **Observed, not read.** `Zigbee2MQTT` is GPL-3.0. Its source has deliberately
//! not been read, and nothing here is translated from it. Every topic and
//! payload below was captured by running `Zigbee2MQTT` against the same
//! coordinator and reading what it put on the wire:
//!
//! ```text
//! zigbee2mqtt/0xa4c138142d62ffff        {"battery":100,"child_lock":"UNLOCK","linkquality":255,"state":"OFF"}
//! zigbee2mqtt/bridge/state              {"state":"online"}
//! zigbee2mqtt/bridge/event              {"data":{"friendly_name":"0x…","ieee_address":"0x…"},"type":"device_joined"}
//! zigbee2mqtt/bridge/response/permit_join   {"data":{"time":254},"status":"ok"}
//! ```
//!
//! and the inbound direction by publishing to it and watching the device
//! react: `zigbee2mqtt/<ieee>/set` with `{"state":"ON"}` opened a valve and
//! produced a state publish with `"state":"ON"`, and `"OFF"` closed it again.
//! An interface reproduced from its observable behaviour is a contract; a
//! translation of an implementation would be a derived work.
//!
//! # Sans-IO
//!
//! No MQTT client here, and no tokio. This crate turns runtime [`Event`]s into
//! [`Publication`]s and inbound [`Message`]s into [`Inbound`] intents, and
//! nothing else. That keeps the contract testable byte for byte against
//! captured payloads without a broker, and it keeps the choice of MQTT library
//! out of the part that has to be exactly right.
//!
//! [`Event`]: rszigbee_core::event::Event
//!
//! # State is cumulative, and that is not a detail
//!
//! `Zigbee2MQTT` publishes a device's *whole* known state on every change, not
//! the field that changed. A consumer reading one message expects a complete
//! picture, and a gateway that published only deltas would look correct in a
//! log and break every consumer that does not accumulate. [`DeviceState`]
//! exists to hold that, and is why this crate is stateful rather than a pure
//! function.

#![forbid(unsafe_code)]

mod inbound;
mod state;
mod topics;

pub use inbound::{Inbound, InboundError, Message, parse};
pub use state::{DeviceState, Publication, StateStore};
pub use topics::Topics;
