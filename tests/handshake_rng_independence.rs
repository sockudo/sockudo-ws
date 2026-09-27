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

#[test]
fn first_handshake_nonce_uses_a_fork_of_the_selected_rng() {
    // A fresh thread gives the nonce generator an uninitialized TLS slot.
    std::thread::spawn(|| {
        use base64::Engine;
        fastrand::seed(42);
        let mut expected_rng = fastrand::Rng::new();
        let mut expected = [0; 16];
        expected_rng.fill(&mut expected);
        let expected_mask = sockudo_ws::mask::generate_mask();

        fastrand::seed(42);
        let key = sockudo_ws::handshake::generate_key();
        let actual = base64::engine::general_purpose::STANDARD
            .decode(key)
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(sockudo_ws::mask::generate_mask(), expected_mask);
    })
    .join()
    .unwrap();
}
