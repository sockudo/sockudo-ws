#![cfg(all(
    feature = "fastrand",
    not(feature = "getrandom"),
    not(feature = "rand_rng")
))]

#[test]
fn handshake_nonce_does_not_advance_the_frame_mask_stream() {
    // Initialize the nonce generator before controlling the mask generator.
    let _ = sockudo_ws::handshake::generate_key();
    fastrand::seed(42);
    let expected = sockudo_ws::mask::generate_mask();

    fastrand::seed(42);
    let _ = sockudo_ws::handshake::generate_key();
    let actual = sockudo_ws::mask::generate_mask();

    assert_eq!(actual, expected);
}
