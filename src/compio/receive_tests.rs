use super::*;
#[cfg(feature = "permessage-deflate")]
use crate::receive_tests::deflate_config;
use crate::receive_tests::{
    batch_input, check_close_is_terminal, check_receive_batches, check_valid_before_parse_failure,
    close_before_parse_failure_input, config, parse_failure_input,
    pending_after_failed_control_reply_input,
};

#[compio::test]
async fn unified_batches_preserve_owned_messages_and_controls() {
    let (io, writes, expected) = batch_input(false);
    let ws = CompioWebSocketStream::client(io, config());
    check_receive_batches!(ws, writes, expected, Vec::new());
}

#[compio::test]
async fn split_batches_preserve_pending_messages_and_controls() {
    let (io, writes, expected) = batch_input(false);
    let mut ws = CompioWebSocketStream::client(io, config());
    let first = ws.next().await.unwrap().unwrap();
    let (reader, _writer) = ws.split();
    check_receive_batches!(reader, writes, expected, vec![first]);
}

#[compio::test]
async fn unified_delivers_valid_message_before_parse_failure() {
    check_valid_before_parse_failure!(CompioWebSocketStream::client(
        parse_failure_input(false),
        config()
    ));
}

#[compio::test]
async fn unified_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(false);
    check_close_is_terminal!(CompioWebSocketStream::client(io, config()));
}

#[compio::test]
async fn unified_write_failure_discards_pending_messages() {
    let mut ws =
        CompioWebSocketStream::client(pending_after_failed_control_reply_input(false), config());

    assert!(ws.next().await.unwrap().is_err());
    assert!(ws.next().await.is_none());
}

#[compio::test]
async fn split_delivers_valid_message_before_parse_failure() {
    let (reader, _writer) =
        CompioWebSocketStream::client(parse_failure_input(false), config()).split();
    check_valid_before_parse_failure!(reader);
}

#[compio::test]
async fn split_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(false);
    let (reader, _writer) = CompioWebSocketStream::client(io, config()).split();
    check_close_is_terminal!(reader);
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_unified_batches_preserve_owned_messages_and_controls() {
    let (io, writes, expected) = batch_input(true);
    let ws = CompioCompressedWebSocketStream::client(io, config(), deflate_config());
    check_receive_batches!(ws, writes, expected, Vec::new());
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_split_batches_preserve_pending_messages_and_controls() {
    let (io, writes, expected) = batch_input(true);
    let mut ws = CompioCompressedWebSocketStream::client(io, config(), deflate_config());
    let first = ws.next().await.unwrap().unwrap();
    let (reader, _writer) = ws.split();
    check_receive_batches!(reader, writes, expected, vec![first]);
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_unified_delivers_valid_message_before_parse_failure() {
    check_valid_before_parse_failure!(CompioCompressedWebSocketStream::client(
        parse_failure_input(true),
        config(),
        deflate_config()
    ));
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_unified_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(true);
    check_close_is_terminal!(CompioCompressedWebSocketStream::client(
        io,
        config(),
        deflate_config()
    ));
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_unified_write_failure_discards_pending_messages() {
    let mut ws = CompioCompressedWebSocketStream::client(
        pending_after_failed_control_reply_input(true),
        config(),
        deflate_config(),
    );

    assert!(ws.next().await.unwrap().is_err());
    assert!(ws.next().await.is_none());
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_split_delivers_valid_message_before_parse_failure() {
    let (reader, _writer) = CompioCompressedWebSocketStream::client(
        parse_failure_input(true),
        config(),
        deflate_config(),
    )
    .split();
    check_valid_before_parse_failure!(reader);
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_split_close_discards_a_later_parse_failure() {
    let (io, _writes) = close_before_parse_failure_input(true);
    let (reader, _writer) =
        CompioCompressedWebSocketStream::client(io, config(), deflate_config()).split();
    check_close_is_terminal!(reader);
}
