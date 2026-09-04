//! Throws malformed input at every decoder that reads bytes off the radio.
//!
//! These four parsers are the library's attack surface. Every byte they see was
//! written by a device nobody controls, and a decoder that panics on a short or
//! contradictory frame takes the whole runtime down from a single malformed
//! packet — a denial of service that needs no privileges beyond being on the
//! network.
//!
//! The README has claimed since the beginning that the sans-IO design makes
//! these cheap to fuzz. Nothing did, which made it a claim rather than a
//! property. This is the cheap version: it runs on stable, in the normal test
//! suite, on every push. `fuzz/` holds coverage-guided targets for longer runs.
//!
//! # What is asserted
//!
//! Only that a decoder returns — `Ok` or `Err`, never a panic. That is
//! deliberately weak, because it is the property that matters and the only one
//! that holds for arbitrary input. Malformed input *should* be rejected; the
//! bug class being hunted is the rejection path itself panicking on a slice
//! index, a subtraction below zero, or a length taken from the input and
//! trusted.
//!
//! Overflow checks are on for this workspace even in release, so arithmetic
//! that wraps is a panic here rather than a silent wrong answer.
//!
//! # Why a hand-rolled PRNG
//!
//! Determinism without a dependency. A failing case must be reproducible from
//! the seed printed in the failure, and a test that pulls in a random-number
//! crate to generate 16 bytes has a worse dependency-to-value ratio than eight
//! lines of xorshift.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use rszigbee_spec::codec::Reader;
use rszigbee_spec::tuya;
use rszigbee_spec::zcl::ZclType;
use rszigbee_spec::zcl::frame::ZclFrame;
use rszigbee_spec::zcl::types::decode_value;
use rszigbee_spec::zdo;

/// xorshift64*, seeded per test so a failure is reproducible.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        // Never zero: xorshift is stuck at zero forever.
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn byte(&mut self) -> u8 {
        // Taken from the middle of the word rather than cast: the low bits of
        // an xorshift are the weakest, and indexing the bytes avoids a
        // truncating cast entirely.
        self.next_u64().to_le_bytes()[3]
    }

    /// A length in `0..=max`, biased towards short inputs.
    ///
    /// Short frames are where truncation bugs live: a decoder that reads a
    /// length byte and then a body is only interesting when the body is not
    /// there.
    fn len(&mut self, max: usize) -> usize {
        let bound = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
        let raw = usize::try_from(self.next_u64() % bound).unwrap_or(0);
        if self.byte() < 96 { raw / 4 } else { raw }
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.len(max);
        (0..n).map(|_| self.byte()).collect()
    }
}

/// Every decoder that takes untrusted bytes, called on one input.
///
/// Grouped into one function so a new decoder is added in a single place and
/// every generator below exercises it.
fn decode_everything(bytes: &[u8]) {
    // A whole ZCL frame, header included: the most likely thing to arrive
    // malformed, since the header's own fields decide how the rest is read.
    let _ = ZclFrame::decode(bytes);

    // ZDO responses. Each has its own length-and-count structure, and the
    // counts come from the frame.
    let _ = zdo::decode_bind_rsp(bytes);
    let _ = zdo::decode_node_desc_rsp(bytes);
    let _ = zdo::decode_active_ep_rsp(bytes);
    let _ = zdo::decode_simple_desc_rsp(bytes);

    // Tuya datapoints: the newest of the four, and the one with a
    // length-prefixed payload per datapoint.
    let _ = tuya::decode(bytes);

    // Every ZCL wire type against this body. `decode_value` is reached with a
    // type tag taken from the frame, so all 256 are reachable from the network
    // whether or not they are meaningful.
    for tag in 0..=u8::MAX {
        let mut reader = Reader::new(bytes);
        let _ = decode_value(ZclType::from_u8(tag), &mut reader);
    }
}

#[test]
fn no_decoder_panics_on_arbitrary_bytes() {
    // Uniform random input. Finds the shallow cases: empty, one byte, a length
    // byte claiming more than is present.
    const SEED: u64 = 0x5a69_6742_6565_4b65;
    const ITERATIONS: usize = 20_000;

    let mut rng = Rng::new(SEED);
    for iteration in 0..ITERATIONS {
        let bytes = rng.bytes(64);
        // The seed and iteration are printed only on failure, via the panic
        // message the decoder itself produces; recording them here would print
        // 20,000 lines on success.
        let _guard = FailureContext {
            seed: SEED,
            iteration,
            bytes: &bytes,
        };
        decode_everything(&bytes);
    }
}

#[test]
fn no_decoder_panics_on_truncations_of_a_valid_frame() {
    // Every prefix of a well-formed frame. This is the shape a real radio
    // produces: a frame cut short by a lost fragment, not random noise. A
    // decoder that reads its length from the frame and then trusts it fails
    // here and nowhere else.
    let valid: &[&[u8]] = &[
        // ZCL read response, genBasic modelId
        &[
            0x18, 0x2a, 0x01, 0x05, 0x00, 0x00, 0x42, 0x07, b'S', b'W', b'V', b'-', b'Z', b'N',
            b'U',
        ],
        // ZDO simple descriptor response, as captured from a real valve
        &[
            0x0c, 0x00, 0xdb, 0xeb, 0x1a, 0x01, 0x04, 0x01, 0x02, 0x00, 0x00, 0x07, 0x00, 0x00,
            0x01, 0x00, 0x03, 0x00, 0x06, 0x00, 0x20, 0x00, 0x57, 0xfc, 0x11, 0xfc, 0x02, 0x03,
            0x00, 0x19, 0x00,
        ],
        // Tuya datapoint report: seq, dp, type, length, value
        &[0x00, 0x01, 0x02, 0x02, 0x00, 0x04, 0x00, 0x00, 0x04, 0xd2],
        // ZDO node descriptor response
        &[
            0x01, 0x00, 0x00, 0x00, 0x02, 0x40, 0x80, 0x86, 0x12, 0x4a, 0x94, 0x01, 0x00, 0x2a,
            0x94, 0x01, 0x00,
        ],
    ];

    for frame in valid {
        for cut in 0..=frame.len() {
            decode_everything(&frame[..cut]);
        }
    }
}

#[test]
fn no_decoder_panics_on_single_byte_mutations_of_a_valid_frame() {
    // Bit flips in an otherwise valid frame. Targets the fields that *drive*
    // parsing — a length, a count, a type tag — where a plausible frame with
    // one wrong number is far more dangerous than noise, because it gets past
    // the early checks and then asks for something that is not there.
    const SEED: u64 = 0x0004_d2f1_0000_0001;

    let base: &[u8] = &[
        0x0c, 0x00, 0xdb, 0xeb, 0x1a, 0x01, 0x04, 0x01, 0x02, 0x00, 0x00, 0x07, 0x00, 0x00, 0x01,
        0x00, 0x03, 0x00, 0x06, 0x00, 0x20, 0x00, 0x57, 0xfc, 0x11, 0xfc, 0x02, 0x03, 0x00, 0x19,
        0x00,
    ];

    let mut rng = Rng::new(SEED);
    for index in 0..base.len() {
        // Exhaustive on the interesting byte values plus a random sample:
        // 0x00, 0xff and 0x7f between them cover "nothing", "everything" and
        // "one below a signed boundary", which is where length arithmetic
        // breaks.
        for value in [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff, rng.byte()] {
            let mut mutated = base.to_vec();
            mutated[index] = value;
            decode_everything(&mutated);
        }
    }
}

#[test]
fn no_decoder_panics_on_pathological_lengths() {
    // Frames whose every byte is a maximal length or count. Constructed rather
    // than sampled, because a uniform generator reaches "every field claims
    // 255 more bytes" only by chance.
    for filler in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
        for len in [0usize, 1, 2, 3, 4, 8, 16, 32, 64, 255, 256] {
            decode_everything(&vec![filler; len]);
        }
    }
}

#[test]
fn the_fuzz_corpus_actually_reaches_the_parsers() {
    // The control for the four tests above. All of them assert only that
    // nothing panics, which a decoder that rejected every input at the first
    // byte would also satisfy — and so would a harness whose inputs were all
    // malformed. Then the suite would be green and nothing would have been
    // exercised.
    //
    // So: the known-good frames must actually decode, and the mutations must
    // produce a mix of accepted and rejected. A generator that only ever
    // produced rejects would fail here.
    let zcl: &[u8] = &[
        0x18, 0x2a, 0x01, 0x05, 0x00, 0x00, 0x42, 0x07, b'S', b'W', b'V', b'-', b'Z', b'N', b'U',
    ];
    assert!(
        ZclFrame::decode(zcl).is_ok(),
        "the known-good ZCL frame must decode, or the fuzz corpus never reaches the parser"
    );

    let simple_desc: &[u8] = &[
        0x0c, 0x00, 0xdb, 0xeb, 0x1a, 0x01, 0x04, 0x01, 0x02, 0x00, 0x00, 0x07, 0x00, 0x00, 0x01,
        0x00, 0x03, 0x00, 0x06, 0x00, 0x20, 0x00, 0x57, 0xfc, 0x11, 0xfc, 0x02, 0x03, 0x00, 0x19,
        0x00,
    ];
    assert!(
        zdo::decode_simple_desc_rsp(simple_desc).is_ok(),
        "the known-good simple descriptor must decode"
    );

    let tuya_report: &[u8] = &[0x00, 0x01, 0x02, 0x02, 0x00, 0x04, 0x00, 0x00, 0x04, 0xd2];
    assert!(
        tuya::decode(tuya_report).is_ok(),
        "the known-good Tuya report must decode"
    );

    // And the mutation generator must straddle the boundary rather than
    // sitting on one side of it.
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    for index in 0..simple_desc.len() {
        for value in [0x00u8, 0x01, 0x7f, 0xff] {
            let mut mutated = simple_desc.to_vec();
            mutated[index] = value;
            if zdo::decode_simple_desc_rsp(&mutated).is_ok() {
                accepted += 1;
            } else {
                rejected += 1;
            }
        }
    }
    println!("single-byte mutations: {accepted} accepted, {rejected} rejected");
    assert!(
        accepted > 0,
        "every mutation was rejected, so the fuzz never gets past validation"
    );
    assert!(
        rejected > 0,
        "no mutation was rejected, so the decoder validates nothing"
    );
}

/// Prints the reproduction case if the enclosing scope panics.
///
/// A `Drop` guard rather than logging every iteration: on success it prints
/// nothing, and on a panic it prints the seed and the exact bytes, which is the
/// difference between a reproducible failure and a rerun of 20,000 random
/// cases.
struct FailureContext<'a> {
    seed: u64,
    iteration: usize,
    bytes: &'a [u8],
}

impl Drop for FailureContext<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            println!(
                "fuzz failure: seed 0x{:016x}, iteration {}, input {:02x?}",
                self.seed, self.iteration, self.bytes
            );
        }
    }
}
