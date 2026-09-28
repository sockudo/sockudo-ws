//! Deterministic synthetic events: changing fields, shared text and varying data.
//! This is a workload model, not a claim about a production traffic distribution.
use bytes::Bytes;
use rand::{RngExt, SeedableRng};

pub fn json_messages(size: usize, count: usize) -> Vec<Bytes> {
    assert!(size >= 256);
    (0..count)
        .map(|index| {
            // Repeated schema, independently changing values; no paired payload copies.
            let mut rng = rand::rngs::StdRng::seed_from_u64(42 + index as u64);
            let mut json = format!("{{\"event\":\"update\",\"seq\":{index},\"records\":[");
            let tail = "],\"note\":\"";
            let mut records = 0;
            loop {
                let record = format!(
                    "{}{{\"id\":{},\"price\":{},\"quantity\":{},\"revision\":{},\"active\":{}}}",
                    if records == 0 { "" } else { "," },
                    rng.random_range(1..1_000_000u32),
                    rng.random_range(1..1_000_000u32),
                    rng.random_range(1..10_000u32),
                    rng.random_range(1..1_000_000u32),
                    rng.random_bool(0.5),
                );
                if json.len() + record.len() + tail.len() + 2 > size {
                    break;
                }
                json.push_str(&record);
                records += 1;
            }
            assert!(records > 0);
            json.push_str(tail);
            // A changing note fills the unused record slot, rather than fixed padding.
            while json.len() + 2 < size {
                json.push(char::from(b'a' + rng.random_range(0..26)));
            }
            json.push_str("\"}");
            assert_eq!(json.len(), size);
            Bytes::from(json)
        })
        .collect()
}

#[cfg(feature = "permessage-deflate")]
pub fn compressed_cycles(payloads: &[Bytes], reset: bool) -> (Vec<Bytes>, Vec<Bytes>) {
    use sockudo_ws::deflate::{DeflateDecoder, DeflateEncoder, MAX_WINDOW_BITS};
    assert!(payloads.iter().map(Bytes::len).sum::<usize>() >= 1usize << u8::from(MAX_WINDOW_BITS));
    let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, reset, 6, 32);
    let [first, repeated] = std::array::from_fn(|_| {
        payloads
            .iter()
            .map(|payload| encoder.compress(payload).unwrap().unwrap())
            .collect::<Vec<_>>()
    });
    let mut decoder = DeflateDecoder::new(MAX_WINDOW_BITS, reset);
    for cycle in [&first, &repeated, &repeated, &repeated] {
        for (wire, expected) in cycle.iter().zip(payloads) {
            assert_eq!(
                decoder.decompress(wire, expected.len()).unwrap().as_ref(),
                expected.as_ref()
            );
        }
    }
    if !reset {
        // Successful cyclic decoding alone does not prove cross-message references.
        assert!(
            repeated.iter().zip(payloads).any(|(wire, expected)| {
                let mut fresh = DeflateDecoder::new(MAX_WINDOW_BITS, false);
                !fresh
                    .decompress(wire, expected.len())
                    .is_ok_and(|actual| actual.as_ref() == expected.as_ref())
            }),
            "fixture must require preceding message history"
        );
    }
    (first, repeated)
}
