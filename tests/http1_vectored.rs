#![cfg(feature = "tokio-runtime")]

#[path = "support/vectored.rs"]
mod vectored;

#[test]
fn http1_preserves_vectored_capability_and_partial_writes() {
    vectored::check_vectored_forwarding(sockudo_ws::Stream::<sockudo_ws::Http1>::new);
}
