//! ZDO responses.
//!
//! Each carries counts and lengths taken from the frame — an endpoint count, a
//! cluster-list length — which the decoder then has to honour without trusting.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rszigbee_spec::zdo;

fuzz_target!(|data: &[u8]| {
    let _ = zdo::decode_bind_rsp(data);
    let _ = zdo::decode_node_desc_rsp(data);
    let _ = zdo::decode_active_ep_rsp(data);
    let _ = zdo::decode_simple_desc_rsp(data);
});
