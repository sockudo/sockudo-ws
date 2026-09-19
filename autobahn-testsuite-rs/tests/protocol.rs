use autobahn_testsuite::{
    catalog,
    codec::{apply_mask, decode_frame, encode_header},
    compression::{Deflate, Parameters},
    config::Spec,
};
use bytes::BytesMut;

#[test]
fn every_catalog_id_is_unique_ordered_and_selectable() {
    let cases = catalog::load().unwrap();
    assert_eq!(cases.len(), 517);
    let ids: std::collections::BTreeSet<_> = cases.iter().map(|c| &c.id).collect();
    assert_eq!(ids.len(), cases.len());
    let nums = cases
        .iter()
        .map(|c| {
            c.id.split('.')
                .map(|n| n.parse::<u32>().unwrap())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert!(nums.windows(2).all(|p| p[0] < p[1]));
    let spec = Spec {
        cases: vec!["6.*".into()],
        exclude_cases: vec!["6.4.*".into()],
        ..Spec::default()
    };
    assert_eq!(spec.selected(&cases).len(), 141);
}
#[test]
fn masking_matches_rfc6455_example_and_chunk_offsets() {
    let key = [0x37, 0xfa, 0x21, 0x3d];
    let mut payload = b"Hello".to_vec();
    apply_mask(&mut payload, key, 0);
    assert_eq!(payload, [0x7f, 0x9f, 0x4d, 0x51, 0x58]);
    apply_mask(&mut payload[..2], key, 0);
    apply_mask(&mut payload[2..], key, 2);
    assert_eq!(payload, b"Hello");
    for len in 0..256 {
        let mut data = (0..len).map(|i| i as u8).collect::<Vec<_>>();
        let expected = data
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[(i + 3) & 3])
            .collect::<Vec<_>>();
        apply_mask(&mut data, key, 3);
        assert_eq!(data, expected);
    }
}
#[test]
fn codec_preserves_partial_input_at_all_header_boundaries() {
    for len in [0, 1, 125, 126, 65535, 65536] {
        for mask in [None, Some([1, 2, 3, 4])] {
            let mut header = [0; 14];
            let n = encode_header(&mut header, true, 0, 2, len, mask);
            let mut data = vec![0x42; len as usize];
            if let Some(key) = mask {
                apply_mask(&mut data, key, 0);
            }
            let mut full = BytesMut::from(&header[..n]);
            full.extend_from_slice(&data);
            for split in 0..n {
                let mut partial = BytesMut::from(&full[..split]);
                let original = partial.clone();
                assert!(
                    decode_frame(&mut partial, mask.is_some(), 1 << 20)
                        .unwrap()
                        .is_none()
                );
                assert_eq!(partial, original);
            }
            let frame = decode_frame(&mut full, mask.is_some(), 1 << 20)
                .unwrap()
                .unwrap();
            assert_eq!(frame.payload.as_ref(), vec![0x42; len as usize]);
            assert!(full.is_empty());
        }
    }
}
#[test]
fn malicious_headers_are_rejected_without_waiting_for_payload() {
    let inputs: [&[u8]; 5] = [
        &[0x82, 127, 0x80, 0, 0, 0, 0, 0, 0, 0],
        &[0x82, 126, 0, 1],
        &[0x89, 126, 0, 126],
        &[0x09, 0],
        &[0x83, 0],
    ];
    for data in inputs {
        assert!(decode_frame(&mut BytesMut::from(data), false, 1024).is_err());
    }
    let mut header = [0; 14];
    let n = encode_header(&mut header, true, 0, 2, 1 << 30, None);
    assert!(decode_frame(&mut BytesMut::from(&header[..n]), false, 1024).is_err());
}
#[test]
fn deflate_decodes_rfc7692_hello_vector() {
    let mut codec = Deflate::new(Parameters::default()).unwrap();
    assert_eq!(
        codec
            .decode(&[0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00], 100)
            .unwrap()
            .as_ref(),
        b"Hello"
    );
}
#[test]
fn compression_handles_context_takeover_windows_and_expansion_limits() {
    for window in [9, 15] {
        for reset in [false, true] {
            let parameters = Parameters {
                tx_window: window,
                rx_window: window,
                tx_no_context: reset,
                rx_no_context: reset,
            };
            let mut tx = Deflate::new(parameters).unwrap();
            let mut rx = Deflate::new(parameters).unwrap();
            for payload in [
                vec![],
                b"Hello".repeat(100),
                vec![0; 100_000],
                (0..65536).map(|i| ((i * 17 + 43) % 256) as u8).collect(),
            ] {
                let encoded = tx.encode(&payload).unwrap();
                assert_eq!(rx.decode(&encoded, 200_000).unwrap().as_ref(), payload);
            }
        }
    }
    let mut tx = Deflate::new(Parameters::default()).unwrap();
    let encoded = tx.encode(&vec![0; 100_000]).unwrap();
    let mut rx = Deflate::new(Parameters::default()).unwrap();
    assert!(rx.decode(&encoded, 1024).is_err());
}
#[test]
fn invalid_configuration_and_case_globs_are_handled() {
    assert!(catalog::matches("6.*.1", "6.24.1"));
    assert!(!catalog::matches("6.*.1", "6.24.11"));
    assert!(
        Spec {
            concurrency: 0,
            ..Spec::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Spec {
            url: "https://localhost".into(),
            ..Spec::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn wamp_serializer_roundtrips_all_reference_messages() {
    let vectors = autobahn_testsuite::serializer::vectors().unwrap();
    assert_eq!(vectors.len(), 40);
    for vector in vectors {
        let json: serde_json::Value = serde_json::from_str(&vector.json).unwrap();
        assert_eq!(json, vector.rmsg);
        let bytes = vector
            .msgpack
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let message: serde_json::Value = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(message, vector.rmsg);
    }
}
