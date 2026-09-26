#![cfg(feature = "tokio-runtime")]

#[path = "support/vectored.rs"]
mod vectored;

#[test]
fn http1_preserves_vectored_capability_and_partial_writes() {
    vectored::check_vectored_forwarding(sockudo_ws::Stream::<sockudo_ws::Http1>::new);
}

#[test]
fn http1_preserves_zero_writes_and_errors() {
    vectored::check_terminal_write_results(sockudo_ws::Stream::<sockudo_ws::Http1>::new);
}
