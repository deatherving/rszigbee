//! Zigbee specification-derived types, data and codecs.
//!
//! This crate is **sans-IO**: no tokio, no serial port, no MQTT, no network. It
//! holds the things the Zigbee specifications define — ZCL frames and data
//! types, the cluster registry, ZDO identifiers, address newtypes — and nothing
//! about how bytes reach it. That constraint is what makes it fuzzable, cheap
//! to test, and reusable by anything that speaks Zigbee.
//!
//! The name is `rszigbee-spec` rather than `rszigbee-zcl` because ZDO is not
//! ZCL, and the crate also carries data types, profile ids and manufacturer
//! codes. It parallels zigbee-herdsman's own `src/zspec/`.
//!
//! # The parse-path invariant
//!
//! Radio frames are untrusted input. Every decoder in this crate returns
//! `Result` and contains no slice indexing, `unwrap`, `expect`, `panic!` or
//! overflowing arithmetic. Malformed input produces an error; it never takes
//! the process down. This is enforced by the workspace clippy configuration and
//! asserted by the fuzz targets.
//!
//! # Example
//!
//! ```
//! use rszigbee_spec::{
//!     ids::{AttrId, ClusterId},
//!     zcl::{ClusterRegistry, ZclFrame, ZclValue},
//! };
//!
//! // An attribute report from a temperature sensor: -10.00 C.
//! let frame = ZclFrame::decode(&[0x18, 0x07, 0x0a, 0x00, 0x04, 0x29, 0x18, 0xfc])?;
//! assert_eq!(frame.header.command.0, 0x0a);
//!
//! // The registry says what type attribute 0x0000 of msTemperatureMeasurement is.
//! let reg = ClusterRegistry::with_builtins();
//! let def = reg.attr(None, ClusterId(0x0402), AttrId(0x0000)).unwrap();
//!
//! let mut r = rszigbee_spec::codec::Reader::new(&[0x18, 0xfc]);
//! assert_eq!(rszigbee_spec::zcl::decode_value(def.ty, &mut r)?, ZclValue::Int(-1000));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

pub mod codec;
pub mod ids;
pub mod zcl;
pub mod zdo;

pub use codec::{CodecError, Reader, Writer};
pub use ids::{
    AttrId, ClusterId, CommandId, EndpointId, GroupId, Ieee, ManufacturerCode, Nwk, ProfileId,
};
