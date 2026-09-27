#![cfg(feature = "permessage-deflate")]
use sockudo_ws::DeflateConfig;

#[test]
fn takeover_directions_are_independent() {
    let config = DeflateConfig::from_params(&[("client_no_context_takeover", None)]).unwrap();
    assert!(config.client_no_context_takeover);
    assert!(!config.server_no_context_takeover);
}

#[test]
fn duplicate_parameters_are_rejected() {
    assert!(
        DeflateConfig::from_params(&[
            ("server_no_context_takeover", None),
            ("server_no_context_takeover", None)
        ])
        .is_err()
    );
}

#[test]
fn server_window_requires_a_value() {
    assert!(DeflateConfig::from_params(&[("server_max_window_bits", None)]).is_err());
}

#[test]
fn unsupported_encoder_window_is_rejected() {
    assert!(DeflateConfig::from_params(&[("server_max_window_bits", Some("8"))]).is_err());
}

#[test]
fn window_bits_with_leading_zeroes_are_rejected() {
    assert!(DeflateConfig::from_params(&[("server_max_window_bits", Some("09"))]).is_err());
}
