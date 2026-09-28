//! Controlled production stream APIs; see benches/README.md.
use criterion::{Criterion, criterion_group, criterion_main};
#[path = "support/stream_cases.rs"]
pub mod stream_cases;
use stream_cases::{Kind, Runtime};
#[path = "support/controlled_io.rs"]
pub mod controlled_io;
#[path = "support/receive_cases.rs"]
pub mod receive_cases;
use receive_cases::receive_case;
fn receive(c: &mut Criterion) {
    receive_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::PlainUnified,
        |io, case: &receive_cases::Case| {
            let ws = sockudo_ws::CompioWebSocketStream::from_raw(io, case.role(), case.config());
            (ws, ())
        }
    );
    receive_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::PlainSplit,
        |io, case: &receive_cases::Case| {
            let ws = sockudo_ws::CompioWebSocketStream::from_raw(io, case.role(), case.config());
            ws.split()
        }
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::CompressedUnified,
        |io, case: &receive_cases::Case| {
            let ws = if case.server {
                sockudo_ws::compio::CompioCompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::compio::CompioCompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            (ws, ())
        }
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::CompressedSplit,
        |io, case: &receive_cases::Case| {
            let ws = if case.server {
                sockudo_ws::compio::CompioCompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::compio::CompioCompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            ws.split()
        }
    );
}
criterion_group!(benches, receive, send);
criterion_main!(benches);

#[path = "support/write_cases.rs"]
pub mod write_cases;
use write_cases::write_case;
fn send(c: &mut Criterion) {
    write_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::PlainUnified,
        send,
        |io, case: &write_cases::Case| {
            let ws = sockudo_ws::CompioWebSocketStream::from_raw(io, case.role(), case.config());
            (ws, ())
        }
    );
    write_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::PlainSplit,
        send,
        |io, case: &write_cases::Case| {
            let ws = sockudo_ws::CompioWebSocketStream::from_raw(io, case.role(), case.config());
            let (reader, writer) = ws.split();
            (writer, reader)
        }
    );
    #[cfg(feature = "permessage-deflate")]
    write_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::CompressedUnified,
        send,
        |io, case: &write_cases::Case| {
            let ws = if case.client {
                sockudo_ws::compio::CompioCompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::compio::CompioCompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            (ws, ())
        }
    );
    #[cfg(feature = "permessage-deflate")]
    write_case!(
        c,
        runtime(),
        Runtime::Compio,
        Kind::CompressedSplit,
        send,
        |io, case: &write_cases::Case| {
            let ws = if case.client {
                sockudo_ws::compio::CompioCompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::compio::CompioCompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            let (reader, writer) = ws.split();
            (writer, reader)
        }
    );
}

#[path = "support/corpus.rs"]
pub mod corpus;

fn runtime() -> compio::runtime::Runtime {
    compio::runtime::Runtime::new().unwrap()
}
