use super::encode_payload_masked_inline;
use std::mem::MaybeUninit;

#[test]
fn copy_mask_preserves_destination_boundaries() {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    for len in 0..=129 {
        for offset in 0..16 {
            // The payload ends at the allocation boundary, without readable padding.
            let source: Box<[u8]> = (0..offset + len).map(|i| (i * 37 + 17) as u8).collect();
            let payload = &source[offset..];
            let mut destination = vec![MaybeUninit::new(0xa5); offset + len + 16];
            encode_payload_masked_inline(&mut destination[offset..offset + len], payload, mask);
            // SAFETY: Sentinels were initialized above; the helper writes every
            // byte of the payload range.
            let destination: Vec<_> = destination
                .into_iter()
                .map(|b| unsafe { b.assume_init() })
                .collect();
            let expected: Vec<_> = payload
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ mask[i & 3])
                .collect();
            assert_eq!(&destination[offset..offset + len], expected);
            assert!(destination[..offset].iter().all(|b| *b == 0xa5));
            assert!(destination[offset + len..].iter().all(|b| *b == 0xa5));
        }
    }
}

#[test]
fn copy_mask_initializes_exact_destination() {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    for len in 0..=129 {
        let source: Box<[u8]> = (0..len).map(|i| (i * 37 + 17) as u8).collect();
        let mut destination = Box::<[u8]>::new_uninit_slice(len);
        encode_payload_masked_inline(&mut destination, &source, mask);
        // SAFETY: The helper initializes every byte, including all remainders.
        let destination = unsafe { destination.assume_init() };
        let expected: Vec<_> = source
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i & 3])
            .collect();
        assert_eq!(&*destination, expected);
    }
}
