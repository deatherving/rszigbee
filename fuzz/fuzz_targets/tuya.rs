//! Tuya datapoint payloads.
//!
//! The newest of the three codecs and the only one with a length prefix *per
//! datapoint*, so a single wrong length walks the cursor past the end of every
//! datapoint after it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rszigbee_spec::tuya;

fuzz_target!(|data: &[u8]| {
    let _ = tuya::decode(data);
});
