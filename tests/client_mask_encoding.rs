use bytes::BytesMut;
use sockudo_ws::frame::{OpCode, encode_frame_with_rsv};

#[test]
fn masked_encoding_matches_wire_oracle_across_independent_alignments() {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    for len in (0..=40).chain([63, 64, 65, 125, 126, 127, 255, 256, 257, 4096, 65535, 65536]) {
        for source_offset in 0..16 {
            let source: Vec<_> = (0..len + 16).map(|i| (i * 37 + 17) as u8).collect();
            let payload = &source[source_offset..source_offset + len];
            for prefix_len in 0..16 {
                // Independent offsets exercise every source/destination phase.
                // Existing destination bytes must survive the appended frame.
                let mut output = BytesMut::with_capacity(prefix_len + len + 14);
                output.resize(prefix_len, 0xa5);
                let mut expected = vec![0xa5; prefix_len];
                // FIN is clear and RSV1 is set to check the shared encoder's
                // flags without depending on a parser accepting compression.
                expected.push(0x42);
                if len <= 125 {
                    expected.push(0x80 | len as u8);
                } else if len <= 65535 {
                    expected.push(0xfe);
                    expected.extend_from_slice(&(len as u16).to_be_bytes());
                } else {
                    expected.push(0xff);
                    expected.extend_from_slice(&(len as u64).to_be_bytes());
                }
                expected.extend_from_slice(&mask);
                expected.extend(
                    payload
                        .iter()
                        .enumerate()
                        .map(|(i, byte)| byte ^ mask[i & 3]),
                );

                encode_frame_with_rsv(
                    &mut output,
                    OpCode::Binary,
                    payload,
                    false,
                    Some(mask),
                    true,
                );

                assert_eq!(
                    output.as_ref(),
                    expected,
                    "length={len}, source={source_offset}, prefix={prefix_len}"
                );
            }
        }
    }
}
