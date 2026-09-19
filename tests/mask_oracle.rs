use sockudo_ws::simd::apply_mask;

#[test]
fn masking_matches_scalar_oracle_for_lengths_and_alignments() {
    for len in (0..=260).chain([
        1023, 1024, 1025, 2047, 2048, 2049, 4095, 4096, 4097, 4099, 65535, 65536, 65543,
    ]) {
        for offset in 0..64 {
            for mask in [[0; 4], [255; 4], [0x37, 0xfa, 0x21, 0x3d], [1, 2, 3, 4]] {
                let mut actual: Vec<_> = (0..len + 64).map(|i| (i * 37 + 17) as u8).collect();
                let mut expected = actual.clone();
                for (index, byte) in expected[offset..offset + len].iter_mut().enumerate() {
                    *byte ^= mask[index & 3];
                }
                apply_mask(&mut actual[offset..offset + len], mask);
                assert_eq!(actual, expected, "length={len}, offset={offset}");
            }
        }
    }
}

#[test]
fn masking_handles_exact_allocations_at_vector_and_tail_boundaries() {
    for len in 0..=260 {
        // Exact allocations let memory checkers detect reads beyond the tail;
        // surrounding sentinels in the alignment test only detect writes.
        let mut data = vec![0x42; len].into_boxed_slice();
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let expected: Vec<_> = (0..len).map(|index| 0x42 ^ mask[index & 3]).collect();

        apply_mask(&mut data, mask);

        assert_eq!(&*data, expected);
    }
}

#[test]
fn split_masking_preserves_phase_without_overlapping_bytes() {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let data: Vec<_> = (0..270).map(|i| i as u8).collect();
    let expected: Vec<_> = data
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ mask[i & 3])
        .collect();
    for split in 0..=data.len() {
        let mut actual = data.clone();
        let (first, rest) = actual.split_at_mut(split);
        apply_mask(first, mask);
        let mut rotated = mask;
        rotated.rotate_left(split & 3);
        apply_mask(rest, rotated);
        assert_eq!(actual, expected, "split={split}");
    }
}
