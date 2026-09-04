//! ZCL frames, header included.
//!
//! The header's own fields decide how the rest of the frame is read — the
//! manufacturer-specific bit adds two bytes, the frame type selects the command
//! namespace — so a contradictory header is the most productive thing to
//! mutate.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rszigbee_spec::codec::Reader;
use rszigbee_spec::zcl::ZclType;
use rszigbee_spec::zcl::frame::ZclFrame;
use rszigbee_spec::zcl::types::decode_value;

fuzz_target!(|data: &[u8]| {
    let _ = ZclFrame::decode(data);

    // Every wire type against this body. A type tag arrives in the frame, so
    // all 256 are reachable from the network whether or not they are
    // meaningful, and the ones with a length prefix are where truncation bites.
    for tag in 0..=u8::MAX {
        let mut reader = Reader::new(data);
        let _ = decode_value(ZclType::from_u8(tag), &mut reader);
    }
});
